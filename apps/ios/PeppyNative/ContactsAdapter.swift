import Foundation

/// The access state is deliberately separate from an empty scan. A limited grant is never
/// authoritative for removals, and a revoked grant is not a contact deletion.
public enum ContactsAccess: String, Sendable, Equatable {
    case full
    case limited
    case denied
    case restricted
    case notDetermined

    public var mayRead: Bool { self == .full || self == .limited }
}

public struct ContactContainer: Sendable, Equatable, Identifiable {
    public let id: String
    public let name: String
    public let isDefault: Bool

    public init(id: String, name: String, isDefault: Bool) {
        self.id = id
        self.name = name
        self.isDefault = isDefault
    }
}

/// One labeled multi-value. `id` is the OS labeled-value identifier (empty for a new value) and
/// `label` is the raw OS label (for example `_$!<Mobile>!$_`), never a localized display string.
public struct ContactField: Sendable, Equatable, Hashable {
    public let id: String
    public let label: String?
    public let value: String

    public init(id: String, label: String?, value: String) {
        self.id = id
        self.label = label
        self.value = value
    }
}

public struct PostalAddressValue: Sendable, Equatable, Hashable {
    public var street = "", subLocality = "", city = "", subAdministrativeArea = ""
    public var state = "", postalCode = "", country = "", isoCountryCode = ""

    public init(
        street: String = "", subLocality: String = "", city: String = "", subAdministrativeArea: String = "",
        state: String = "", postalCode: String = "", country: String = "", isoCountryCode: String = ""
    ) {
        self.street = street; self.subLocality = subLocality; self.city = city; self.subAdministrativeArea = subAdministrativeArea
        self.state = state; self.postalCode = postalCode; self.country = country; self.isoCountryCode = isoCountryCode
    }
}

public struct ContactAddressField: Sendable, Equatable, Hashable {
    public let id: String
    public let label: String?
    public let value: PostalAddressValue

    public init(id: String, label: String?, value: PostalAddressValue) {
        self.id = id
        self.label = label
        self.value = value
    }
}

/// Gregorian birthday; any component may be absent (for example a birthday without a year).
public struct ContactBirthday: Sendable, Equatable, Hashable {
    public let year: Int?
    public let month: Int?
    public let day: Int?

    public init(year: Int? = nil, month: Int? = nil, day: Int? = nil) {
        self.year = year
        self.month = month
        self.day = day
    }
}

/// Common iOS-safe Contact data for ONE underlying (non-unified) contact in ONE container.
/// Notes are intentionally absent: CNContact.note requires an entitlement this app does not hold.
/// Photo bytes are never part of a scan; `hasImage` is reported and bytes are read lazily.
public struct PlatformContact: Sendable, Equatable, Identifiable {
    public let id: String
    public var containerId: String?
    public var givenName: String
    public var middleName: String
    public var familyName: String
    public var namePrefix: String
    public var nameSuffix: String
    public var nickname: String
    public var organization: String
    public var jobTitle: String
    public var phones: [ContactField]
    public var emails: [ContactField]
    public var postalAddresses: [ContactAddressField]
    public var birthday: ContactBirthday?
    public var hasImage: Bool

    public init(
        id: String, containerId: String? = nil, givenName: String = "", middleName: String = "",
        familyName: String = "", namePrefix: String = "", nameSuffix: String = "", nickname: String = "",
        organization: String = "", jobTitle: String = "", phones: [ContactField] = [],
        emails: [ContactField] = [], postalAddresses: [ContactAddressField] = [],
        birthday: ContactBirthday? = nil, hasImage: Bool = false
    ) {
        self.id = id; self.containerId = containerId; self.givenName = givenName; self.middleName = middleName
        self.familyName = familyName; self.namePrefix = namePrefix; self.nameSuffix = nameSuffix; self.nickname = nickname
        self.organization = organization; self.jobTitle = jobTitle; self.phones = phones; self.emails = emails
        self.postalAddresses = postalAddresses; self.birthday = birthday; self.hasImage = hasImage
    }
}

public enum ContactsLimits {
    public static let maxContacts = 20_000
    /// Matches the core's photo decode-input bound; larger OS images are reported, not loaded into a write.
    public static let maxPhotoBytes = 8 * 1024 * 1024
}

/// Photo intent for a write. `.keep` leaves the existing OS photo untouched.
public enum ContactPhotoChange: Sendable, Equatable {
    case keep
    case set(Data)
    case remove
}

public enum ContactPhotoRead: Sendable, Equatable {
    case none
    case data(Data)
    case tooLarge(byteCount: Int)
}

/// Writes target one underlying contact identifier, never a unified aggregate.
/// For update, modeled fields describe the full desired state; list items keep their OS identity by `id`.
public enum ContactsWrite: Sendable, Equatable {
    case create(contact: PlatformContact, containerId: String?, photo: ContactPhotoChange)
    case update(id: String, contact: PlatformContact, photo: ContactPhotoChange)
    case delete(id: String)
}

public enum ContactsWriteResult: Sendable, Equatable {
    /// The write was saved; for create, `id` is the new underlying contact identifier.
    case applied(id: String)
    /// The save may or may not have landed. The core must reconcile against OS evidence; never retry blindly.
    case outcomeUnknown
    case outOfGrant
    case readOnly
    case notFound
}

public enum ContactsAdapterError: Error, Equatable {
    case photoTooLarge(byteCount: Int)
}

public struct ContactsFetch: Sendable, Equatable {
    public let contacts: [PlatformContact]
    /// False when enumeration stopped early (cancellation or the contact bound).
    public let complete: Bool
    public let limitExceeded: Bool

    public init(contacts: [PlatformContact], complete: Bool, limitExceeded: Bool = false) {
        self.contacts = contacts
        self.complete = complete
        self.limitExceeded = limitExceeded
    }
}

public struct ContactsPage: Sendable {
    public let contacts: [PlatformContact]
    public let nextCursor: String?
    public let hasMore: Bool
    /// The identifier scope and this page were read completely, independently of `hasMore`.
    public let complete: Bool
    public let limitExceeded: Bool

    public init(contacts: [PlatformContact], nextCursor: String?, hasMore: Bool, complete: Bool, limitExceeded: Bool = false) {
        self.contacts = contacts; self.nextCursor = nextCursor; self.hasMore = hasMore
        self.complete = complete; self.limitExceeded = limitExceeded
    }
}

/// Native boundary used by the contact pass. Tests use fakes; production uses CNContactStore.
/// It owns no durable tokens, maps, ledgers, or plaintext queue.
public protocol ContactsProvider: Sendable {
    func access() async throws -> ContactsAccess
    func containers() async throws -> [ContactContainer]
    func fetchAll(isCancelled: @Sendable () -> Bool) async throws -> ContactsFetch
    func fetchPage(after: String?, limit: Int, isCancelled: @Sendable () -> Bool) async throws -> ContactsPage
    /// Rereads one underlying contact (post-write evidence); nil when it is absent or out of grant.
    func contact(id: String) async throws -> PlatformContact?
    /// Opaque OS change-history token, or nil when unavailable. Equal tokens mean no contact changes.
    func historyToken() async -> Data?
    func photo(id: String, thumbnail: Bool) async throws -> ContactPhotoRead
    func save(_ write: ContactsWrite) async throws -> ContactsWriteResult
    /// Added/updated underlying contacts since `token`, or `.fullScanRequired`.
    func changes(since token: Data, limit: Int) async -> ContactsHistoryChanges
    /// Underlying contact identifiers whose name matches (create evidence); nil when unsupported.
    func identifiers(matchingName name: String) async throws -> [String]?
}

/// Incremental OS change history. Anything the fast path cannot prove complete and deletion-free
/// falls back to the bounded full scan, whose deletion policy is the only one that removes contacts.
public enum ContactsHistoryChanges: Sendable, Equatable {
    case changed(ids: [String], token: Data)
    case fullScanRequired
}

public extension ContactsProvider {
    func fetchPage(after: String?, limit: Int, isCancelled: @Sendable () -> Bool) async throws -> ContactsPage {
        let all = try await fetchAll(isCancelled: isCancelled)
        let remaining = all.contacts.sorted { $0.id < $1.id }.filter { contact in after.map { contact.id > $0 } ?? true }
        let page = Array(remaining.prefix(max(1, limit)))
        return ContactsPage(contacts: page, nextCursor: page.last?.id ?? after,
                            hasMore: remaining.count > page.count || !all.complete,
                            complete: all.complete, limitExceeded: all.limitExceeded)
    }
    func changes(since token: Data, limit: Int) async -> ContactsHistoryChanges { .fullScanRequired }
    func identifiers(matchingName name: String) async throws -> [String]? { nil }
}

public enum ContactsPassReason: Sendable, Equatable { case foreground, changeNotification, backgroundRefresh, backgroundProcessing }

public struct ContactsPassInput: Sendable, Equatable {
    public let reason: ContactsPassReason
    public let access: ContactsAccess
    public let contacts: [PlatformContact]
    public let containers: [ContactContainer]
    public let cancelled: Bool
    /// The contact bound was hit; the scan is partial and the book needs attention, not deletions.
    public let limitExceeded: Bool
    /// True only for a fully enumerated, uncancelled scan with full access before and after the fetch.
    /// Only an authoritative scan may let the core infer removals from absence.
    public let authoritative: Bool

    public init(
        reason: ContactsPassReason, access: ContactsAccess, contacts: [PlatformContact], containers: [ContactContainer],
        cancelled: Bool, limitExceeded: Bool = false, authoritative: Bool
    ) {
        self.reason = reason; self.access = access; self.contacts = contacts; self.containers = containers
        self.cancelled = cancelled; self.limitExceeded = limitExceeded; self.authoritative = authoritative
    }
}

/// A bounded source pass. The caller supplies the real core bridge; there is intentionally no
/// success fallback when bindings are unavailable. Private contact values are never logged here.
public struct ContactsPass: Sendable {
    public let provider: any ContactsProvider

    public init(provider: any ContactsProvider) { self.provider = provider }

    public func capture(reason: ContactsPassReason, isCancelled: @Sendable () -> Bool = { false }) async throws -> ContactsPassInput {
        let access = try await provider.access()
        guard access.mayRead, !isCancelled() else {
            return ContactsPassInput(reason: reason, access: access, contacts: [], containers: [], cancelled: isCancelled(), authoritative: false)
        }
        let containers = try await provider.containers()
        guard !isCancelled() else {
            return ContactsPassInput(reason: reason, access: access, contacts: [], containers: containers, cancelled: true, authoritative: false)
        }
        let fetch = try await provider.fetchAll(isCancelled: isCancelled)
        let wasCancelled = isCancelled() || (!fetch.complete && !fetch.limitExceeded)
        let currentAccess = try await provider.access()
        let authoritative = access == .full && currentAccess == .full && fetch.complete && !wasCancelled
        return ContactsPassInput(
            reason: reason, access: currentAccess, contacts: fetch.contacts, containers: containers,
            cancelled: wasCancelled, limitExceeded: fetch.limitExceeded, authoritative: authoritative
        )
    }
}

#if canImport(Contacts)
@preconcurrency import Contacts
#if canImport(PeppyContactsHistory)
import PeppyContactsHistory
#endif

@available(iOS 13.0, *)
public final class CNContactStoreProvider: ContactsProvider, @unchecked Sendable {
    private let store: CNContactStore

    public init(store: CNContactStore = CNContactStore()) { self.store = store }

    public func access() async throws -> ContactsAccess {
        switch CNContactStore.authorizationStatus(for: .contacts) {
        case .authorized: return .full
        case .limited: return .limited
        case .denied: return .denied
        case .restricted: return .restricted
        case .notDetermined: return .notDetermined
        @unknown default: return .restricted
        }
    }

    public func containers() async throws -> [ContactContainer] {
        let defaultId = store.defaultContainerIdentifier()
        return try store.containers(matching: nil).map { ContactContainer(id: $0.identifier, name: $0.name, isDefault: $0.identifier == defaultId) }
    }

    /// Enumerates underlying (non-unified) contacts container by container, so every record carries its
    /// source container. Photo bytes are not loaded; `imageDataAvailable` is reported instead.
    public func fetchAll(isCancelled: @Sendable () -> Bool) async throws -> ContactsFetch {
        let containerIds = try store.containers(matching: nil).map(\.identifier)
        var result: [PlatformContact] = []
        var stoppedEarly = false
        var limitExceeded = false
        for containerId in containerIds {
            if isCancelled() { stoppedEarly = true; break }
            let request = CNContactFetchRequest(keysToFetch: ContactsConversion.scanKeys)
            request.unifyResults = false
            request.predicate = CNContact.predicateForContactsInContainer(withIdentifier: containerId)
            try store.enumerateContacts(with: request) { contact, stop in
                if result.count >= ContactsLimits.maxContacts {
                    limitExceeded = true; stoppedEarly = true; stop.pointee = true; return
                }
                result.append(ContactsConversion.platform(contact, containerId: containerId))
                if isCancelled() { stoppedEarly = true; stop.pointee = true }
            }
            if stoppedEarly { break }
        }
        return ContactsFetch(contacts: result, complete: !stoppedEarly, limitExceeded: limitExceeded)
    }

    public func contact(id: String) async throws -> PlatformContact? {
        guard let found = try underlying(id, keys: ContactsConversion.scanKeys) else { return nil }
        let container = try store.containers(matching: CNContainer.predicateForContainerOfContact(withIdentifier: id)).first
        return ContactsConversion.platform(found, containerId: container?.identifier)
    }

    /// Index only identifiers, then load a bounded slice of full records. Repeated passes do
    /// not re-read all the earlier contacts' fields to reach their durable source-id cursor.
    public func fetchPage(after: String?, limit: Int, isCancelled: @Sendable () -> Bool) async throws -> ContactsPage {
        let deadline = ProcessInfo.processInfo.systemUptime + 5
        var ids: [String] = []
        var indexComplete = true
        var limitExceeded = false
        let request = CNContactFetchRequest(keysToFetch: [CNContactIdentifierKey as CNKeyDescriptor])
        request.unifyResults = false
        try store.enumerateContacts(with: request) { contact, stop in
            if isCancelled() { indexComplete = false; stop.pointee = true; return }
            if ids.count >= ContactsLimits.maxContacts {
                indexComplete = false; limitExceeded = true; stop.pointee = true; return
            }
            ids.append(contact.identifier)
        }
        guard !isCancelled() else {
            return ContactsPage(contacts: [], nextCursor: after, hasMore: true, complete: false)
        }
        let remaining = ids.sorted().filter { id in after.map { id > $0 } ?? true }
        var contacts: [PlatformContact] = []
        var cursor = after
        var examined = 0
        var complete = indexComplete
        for id in remaining.prefix(max(1, limit)) {
            if isCancelled() || (examined > 0 && ProcessInfo.processInfo.systemUptime >= deadline) { break }
            if let value = try await contact(id: id) { contacts.append(value) }
            else { complete = false }
            cursor = id
            examined += 1
        }
        return ContactsPage(contacts: contacts, nextCursor: cursor,
                            hasMore: examined < remaining.count || isCancelled(),
                            complete: complete && !isCancelled(), limitExceeded: limitExceeded)
    }

    public func historyToken() async -> Data? { store.currentHistoryToken }

    /// Uses the Objective-C wrapper (the enumerator is NS_SWIFT_UNAVAILABLE). Deletions,
    /// drop-everything, errors and more than `limit` changed contacts require a full scan.
    public func changes(since token: Data, limit: Int) async -> ContactsHistoryChanges {
        guard let result = try? PeppyContactsHistory.fetchChanges(in: store, since: token, keys: [CNContactIdentifierKey as CNKeyDescriptor]) else {
            return .fullScanRequired
        }
        var ids: [String] = []
        var seen = Set<String>()
        let events = result.value
        while let event = events.nextObject() {
            let id: String
            switch event {
            case let added as CNChangeHistoryAddContactEvent: id = added.contact.identifier
            case let updated as CNChangeHistoryUpdateContactEvent: id = updated.contact.identifier
            case is CNChangeHistoryDeleteContactEvent, is CNChangeHistoryDropEverythingEvent: return .fullScanRequired
            default: continue
            }
            if seen.insert(id).inserted {
                ids.append(id)
                if ids.count > limit { return .fullScanRequired }
            }
        }
        // Read after enumeration: the token then covers every event returned above.
        return .changed(ids: ids, token: result.currentHistoryToken)
    }

    public func identifiers(matchingName name: String) async throws -> [String]? {
        guard !name.isEmpty else { return [] }
        let request = CNContactFetchRequest(keysToFetch: [CNContactIdentifierKey as CNKeyDescriptor])
        request.unifyResults = false
        request.predicate = CNContact.predicateForContacts(matchingName: name)
        var ids: [String] = []
        try store.enumerateContacts(with: request) { contact, stop in
            ids.append(contact.identifier)
            if ids.count > 200 { stop.pointee = true }
        }
        return ids
    }

    /// Lazily reads one contact's photo. Full images above the decode bound are reported, not returned.
    public func photo(id: String, thumbnail: Bool) async throws -> ContactPhotoRead {
        let key = (thumbnail ? CNContactThumbnailImageDataKey : CNContactImageDataKey) as CNKeyDescriptor
        guard let contact = try underlying(id, keys: [key]) else { return .none }
        guard let data = thumbnail ? contact.thumbnailImageData : contact.imageData else { return .none }
        return data.count > ContactsLimits.maxPhotoBytes ? .tooLarge(byteCount: data.count) : .data(data)
    }

    public func save(_ write: ContactsWrite) async throws -> ContactsWriteResult {
        let access = try await access()
        guard access.mayRead else { return .outOfGrant }
        do {
            return try perform(write, access: access)
        } catch let error as CNError {
            return try ContactsConversion.result(for: error, access: access)
        }
    }

    private func perform(_ write: ContactsWrite, access: ContactsAccess) throws -> ContactsWriteResult {
        // Under a limited grant an invisible record is out of grant, not proof that it was deleted.
        let missing: ContactsWriteResult = access == .limited ? .outOfGrant : .notFound
        let request = CNSaveRequest()
        switch write {
        case let .create(contact, containerId, photo):
            let mutable = CNMutableContact()
            try ContactsConversion.apply(contact, photo: photo, to: mutable)
            request.add(mutable, toContainerWithIdentifier: containerId)
            try store.execute(request)
            // The save succeeded; confirm the identifier with OS evidence. A failed or empty confirmation
            // is reported as unknown so the core reconciles instead of creating a duplicate.
            let confirmed = (try? underlying(mutable.identifier, keys: [CNContactIdentifierKey as CNKeyDescriptor])) ?? nil
            return confirmed != nil ? .applied(id: mutable.identifier) : .outcomeUnknown
        case let .update(id, contact, photo):
            guard let existing = try underlying(id, keys: ContactsConversion.writeKeys(photo: photo)) else { return missing }
            let mutable = existing.mutableCopy() as! CNMutableContact
            try ContactsConversion.apply(contact, photo: photo, to: mutable)
            request.update(mutable)
            try store.execute(request)
            return .applied(id: id)
        case let .delete(id):
            guard let existing = try underlying(id, keys: [CNContactIdentifierKey as CNKeyDescriptor]) else { return missing }
            request.delete(existing.mutableCopy() as! CNMutableContact)
            try store.execute(request)
            return .applied(id: id)
        }
    }

    /// Fetches exactly one underlying contact by its own identifier; never a unified aggregate.
    private func underlying(_ id: String, keys: [CNKeyDescriptor]) throws -> CNContact? {
        let request = CNContactFetchRequest(keysToFetch: keys)
        request.unifyResults = false
        request.predicate = CNContact.predicateForContacts(withIdentifiers: [id])
        var found: CNContact?
        try store.enumerateContacts(with: request) { contact, stop in
            if contact.identifier == id { found = contact; stop.pointee = true }
        }
        return found
    }
}

/// Store-free conversion between CNContact and PlatformContact. Tests exercise it with in-memory contacts.
enum ContactsConversion {
    static let scanKeys: [CNKeyDescriptor] = [
        CNContactIdentifierKey, CNContactGivenNameKey, CNContactMiddleNameKey, CNContactFamilyNameKey,
        CNContactNamePrefixKey, CNContactNameSuffixKey, CNContactNicknameKey, CNContactOrganizationNameKey,
        CNContactJobTitleKey, CNContactPhoneNumbersKey, CNContactEmailAddressesKey, CNContactPostalAddressesKey,
        CNContactBirthdayKey, CNContactImageDataAvailableKey,
    ].map { $0 as CNKeyDescriptor }

    /// Every key `apply` may mutate must be fetched before update.
    static func writeKeys(photo: ContactPhotoChange) -> [CNKeyDescriptor] {
        photo == .keep ? scanKeys : scanKeys + [CNContactImageDataKey as CNKeyDescriptor]
    }

    static func platform(_ c: CNContact, containerId: String?) -> PlatformContact {
        PlatformContact(
            id: c.identifier, containerId: containerId, givenName: c.givenName, middleName: c.middleName,
            familyName: c.familyName, namePrefix: c.namePrefix, nameSuffix: c.nameSuffix, nickname: c.nickname,
            organization: c.organizationName, jobTitle: c.jobTitle,
            phones: c.phoneNumbers.map { ContactField(id: $0.identifier, label: $0.label, value: $0.value.stringValue) },
            emails: c.emailAddresses.map { ContactField(id: $0.identifier, label: $0.label, value: $0.value as String) },
            postalAddresses: c.postalAddresses.map { ContactAddressField(id: $0.identifier, label: $0.label, value: address($0.value)) },
            birthday: c.birthday.map { ContactBirthday(year: $0.year, month: $0.month, day: $0.day) },
            hasImage: c.imageDataAvailable
        )
    }

    /// Applies desired modeled state. Unchanged scalars are not reassigned; list items matched by `id`
    /// keep their CNLabeledValue identity; the photo changes only on `.set`/`.remove`.
    static func apply(_ v: PlatformContact, photo: ContactPhotoChange, to c: CNMutableContact) throws {
        if c.givenName != v.givenName { c.givenName = v.givenName }
        if c.middleName != v.middleName { c.middleName = v.middleName }
        if c.familyName != v.familyName { c.familyName = v.familyName }
        if c.namePrefix != v.namePrefix { c.namePrefix = v.namePrefix }
        if c.nameSuffix != v.nameSuffix { c.nameSuffix = v.nameSuffix }
        if c.nickname != v.nickname { c.nickname = v.nickname }
        if c.organizationName != v.organization { c.organizationName = v.organization }
        if c.jobTitle != v.jobTitle { c.jobTitle = v.jobTitle }
        c.phoneNumbers = merge(c.phoneNumbers, v.phones.map { ($0.id, $0.label, $0.value) },
                               same: { $0.stringValue == $1 }, make: { CNPhoneNumber(stringValue: $0) })
        c.emailAddresses = merge(c.emailAddresses, v.emails.map { ($0.id, $0.label, $0.value) },
                                 same: { ($0 as String) == $1 }, make: { $0 as NSString })
        c.postalAddresses = merge(c.postalAddresses, v.postalAddresses.map { ($0.id, $0.label, $0.value) },
                                  same: { address($0) == $1 }, make: { postal($0) })
        let birthday = v.birthday.map { DateComponents(year: $0.year, month: $0.month, day: $0.day) }
        if c.birthday.map({ ContactBirthday(year: $0.year, month: $0.month, day: $0.day) }) != v.birthday { c.birthday = birthday }
        switch photo {
        case .keep: break
        case .remove: c.imageData = nil
        case let .set(data):
            guard data.count <= ContactsLimits.maxPhotoBytes else { throw ContactsAdapterError.photoTooLarge(byteCount: data.count) }
            c.imageData = data
        }
    }

    /// Desired list order wins. A desired item whose id matches an existing labeled value reuses that object
    /// (unchanged) or derives from it with settingLabel/settingValue (changed), preserving its identifier.
    /// Items with an empty or unknown id are new; existing items absent from the desired list are removed.
    static func merge<V, D>(
        _ existing: [CNLabeledValue<V>], _ desired: [(id: String, label: String?, value: D)],
        same: (V, D) -> Bool, make: (D) -> V
    ) -> [CNLabeledValue<V>] where V: NSCopying & NSSecureCoding {
        let byId = Dictionary(existing.map { ($0.identifier, $0) }, uniquingKeysWith: { first, _ in first })
        var used = Set<String>()
        return desired.map { item in
            guard !item.id.isEmpty, let old = byId[item.id], used.insert(item.id).inserted else {
                return CNLabeledValue(label: item.label, value: make(item.value))
            }
            switch (old.label == item.label, same(old.value, item.value)) {
            case (true, true): return old
            case (false, true): return old.settingLabel(item.label)
            case (true, false): return old.settingValue(make(item.value))
            case (false, false): return old.settingLabel(item.label, value: make(item.value))
            }
        }
    }

    static func address(_ a: CNPostalAddress) -> PostalAddressValue {
        PostalAddressValue(
            street: a.street, subLocality: a.subLocality, city: a.city, subAdministrativeArea: a.subAdministrativeArea,
            state: a.state, postalCode: a.postalCode, country: a.country, isoCountryCode: a.isoCountryCode
        )
    }

    static func postal(_ v: PostalAddressValue) -> CNPostalAddress {
        let a = CNMutablePostalAddress()
        a.street = v.street; a.subLocality = v.subLocality; a.city = v.city; a.subAdministrativeArea = v.subAdministrativeArea
        a.state = v.state; a.postalCode = v.postalCode; a.country = v.country; a.isoCountryCode = v.isoCountryCode
        return a.copy() as! CNPostalAddress
    }

    /// Maps Contacts framework write failures to provider outcomes; anything else is rethrown.
    static func result(for error: CNError, access: ContactsAccess) throws -> ContactsWriteResult {
        switch error.code {
        case .recordDoesNotExist, .recordIdentifierInvalid:
            return access == .limited ? .outOfGrant : .notFound
        case .authorizationDenied, .noAccessableWritableContainers:
            return .outOfGrant
        case .recordNotWritable, .parentContainerNotWritable, .policyViolation:
            return .readOnly
        default:
            throw error
        }
    }
}
#endif
