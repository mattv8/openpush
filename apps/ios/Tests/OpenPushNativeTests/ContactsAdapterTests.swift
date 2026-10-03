import Foundation
import Testing
@testable import OpenPushNative

private final class Flag: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false
    func get() -> Bool { lock.withLock { value } }
    func set(_ newValue: Bool) { lock.withLock { value = newValue } }
}

private actor FakeContactsProvider: ContactsProvider {
    var state: ContactsAccess = .full
    var values: [PlatformContact] = []
    var complete = true
    var limitExceeded = false
    /// Access reported by every call after the fetch, to simulate a mid-scan downgrade.
    var stateAfterFetch: ContactsAccess?
    var saves: [ContactsWrite] = []
    func access() async throws -> ContactsAccess { state }
    func containers() async throws -> [ContactContainer] { [ContactContainer(id: "default", name: "Personal", isDefault: true)] }
    func fetchAll(isCancelled: @Sendable () -> Bool) async throws -> ContactsFetch {
        var out: [PlatformContact] = []
        var stopped = false
        for value in values {
            out.append(value)
            if isCancelled() { stopped = true; break }
        }
        if let stateAfterFetch { state = stateAfterFetch }
        return ContactsFetch(contacts: out, complete: complete && !stopped, limitExceeded: limitExceeded)
    }
    func photo(id: String, thumbnail: Bool) async throws -> ContactPhotoRead { .none }
    func contact(id: String) async throws -> PlatformContact? { values.first { $0.id == id } }
    func historyToken() async -> Data? { nil }
    func save(_ write: ContactsWrite) async throws -> ContactsWriteResult { saves.append(write); return .applied(id: "fake") }
    func configure(_ body: @Sendable (isolated FakeContactsProvider) -> Void) { body(self) }
}

@Suite struct ContactsAdapterTests {
    @Test func limitedAccessIsCapturedButNotAuthoritative() async throws {
        let fake = FakeContactsProvider()
        await fake.configure { $0.state = .limited; $0.values = [PlatformContact(id: "c1", givenName: "Private")] }
        let input = try await ContactsPass(provider: fake).capture(reason: .foreground)
        #expect(input.access == .limited)
        #expect(input.contacts.count == 1)
        #expect(!input.authoritative)
    }

    @Test func revokedAccessDoesNotPretendTheBookIsEmpty() async throws {
        let fake = FakeContactsProvider()
        await fake.configure { $0.state = .denied }
        let input = try await ContactsPass(provider: fake).capture(reason: .backgroundRefresh)
        #expect(input.access == .denied)
        #expect(input.contacts.isEmpty && input.containers.isEmpty)
        #expect(!input.access.mayRead)
        #expect(!input.authoritative)
    }

    @Test func cancellationStopsBeforeFetchingContacts() async throws {
        let fake = FakeContactsProvider()
        let input = try await ContactsPass(provider: fake).capture(reason: .changeNotification, isCancelled: { true })
        #expect(input.cancelled)
        #expect(input.contacts.isEmpty)
        #expect(!input.authoritative)
    }

    @Test func fullPermissionCompleteScansAreAuthoritative() async throws {
        let fake = FakeContactsProvider()
        await fake.configure { $0.values = [PlatformContact(id: "c1", givenName: "Alice")] }
        let input = try await ContactsPass(provider: fake).capture(reason: .foreground)
        #expect(input.access == .full)
        #expect(input.contacts.count == 1)
        #expect(input.authoritative)
    }

    @Test func cancellationDuringFetchIsIncompleteAndNotAuthoritative() async throws {
        let fake = FakeContactsProvider()
        await fake.configure { $0.values = (0..<5).map { PlatformContact(id: "c\($0)") } }
        let flag = Flag()
        // Cancel only once the provider is enumerating (third probe): access and containers already passed.
        let counter = Counter()
        let input = try await ContactsPass(provider: fake).capture(reason: .backgroundRefresh, isCancelled: {
            if counter.next() >= 3 { flag.set(true) }
            return flag.get()
        })
        #expect(input.access == .full)
        #expect(input.contacts.count < 5)
        #expect(input.cancelled)
        #expect(!input.authoritative)
    }

    @Test func incompleteFetchWithoutCancellationIsNotAuthoritative() async throws {
        let fake = FakeContactsProvider()
        await fake.configure { $0.values = [PlatformContact(id: "c1")]; $0.complete = false }
        let input = try await ContactsPass(provider: fake).capture(reason: .foreground)
        #expect(!input.authoritative)
        #expect(input.cancelled)
    }

    @Test func contactBoundIsReportedAndNotAuthoritative() async throws {
        let fake = FakeContactsProvider()
        await fake.configure { $0.values = [PlatformContact(id: "c1")]; $0.complete = false; $0.limitExceeded = true }
        let input = try await ContactsPass(provider: fake).capture(reason: .backgroundProcessing)
        #expect(input.limitExceeded)
        #expect(!input.cancelled)
        #expect(!input.authoritative)
    }

    @Test func accessDowngradeDuringScanIsNotAuthoritative() async throws {
        let fake = FakeContactsProvider()
        await fake.configure { $0.values = [PlatformContact(id: "c1")]; $0.stateAfterFetch = .limited }
        let input = try await ContactsPass(provider: fake).capture(reason: .foreground)
        #expect(input.access == .limited)
        #expect(!input.authoritative)
    }
}

private final class Counter: @unchecked Sendable {
    private let lock = NSLock()
    private var count = 0
    func next() -> Int { lock.withLock { count += 1; return count } }
}

#if canImport(Contacts)
import Contacts

@Suite struct ContactsConversionTests {
    private func sample() -> CNMutableContact {
        let c = CNMutableContact()
        c.givenName = "Ada"; c.familyName = "Lovelace"
        c.phoneNumbers = [
            CNLabeledValue(label: CNLabelPhoneNumberMobile, value: CNPhoneNumber(stringValue: "+12025550100")),
            CNLabeledValue(label: CNLabelHome, value: CNPhoneNumber(stringValue: "+12025550101")),
            CNLabeledValue(label: CNLabelWork, value: CNPhoneNumber(stringValue: "+12025550102")),
        ]
        c.emailAddresses = [CNLabeledValue(label: CNLabelHome, value: "ada@example.com" as NSString)]
        let address = CNMutablePostalAddress(); address.street = "1 Analytical Way"; address.city = "London"; address.isoCountryCode = "gb"
        c.postalAddresses = [CNLabeledValue(label: CNLabelHome, value: address.copy() as! CNPostalAddress)]
        c.birthday = DateComponents(month: 12, day: 10)
        c.imageData = Data([0xFF, 0xD8, 0xFF])
        return c
    }

    @Test func scanKeepsRawLabelsIdsAddressesAndPartialBirthday() {
        let c = sample()
        let p = ContactsConversion.platform(c, containerId: "icloud")
        #expect(p.containerId == "icloud")
        #expect(p.phones.map(\.label) == [CNLabelPhoneNumberMobile, CNLabelHome, CNLabelWork])
        #expect(p.phones.map(\.id) == c.phoneNumbers.map(\.identifier))
        #expect(p.postalAddresses.first?.value.street == "1 Analytical Way")
        #expect(p.postalAddresses.first?.value.isoCountryCode == "gb")
        #expect(p.birthday == ContactBirthday(year: nil, month: 12, day: 10))
    }

    @Test func roundTripPreservesUnchangedItemsAndIdentity() throws {
        let c = sample()
        let before = c.phoneNumbers
        var desired = ContactsConversion.platform(c, containerId: nil)
        try ContactsConversion.apply(desired, photo: .keep, to: c)
        #expect(c.phoneNumbers.map(\.identifier) == before.map(\.identifier))
        #expect(zip(c.phoneNumbers, before).allSatisfy { $0 === $1 })
        #expect(c.phoneNumbers.map(\.label) == before.map(\.label))
        #expect(c.imageData == Data([0xFF, 0xD8, 0xFF]))

        // Edit value of #1, relabel #0, drop #2, add a new number.
        desired.phones = [
            ContactField(id: before[0].identifier, label: CNLabelWork, value: "+12025550100"),
            ContactField(id: before[1].identifier, label: CNLabelHome, value: "+12025550199"),
            ContactField(id: "", label: CNLabelPhoneNumberiPhone, value: "+12025550103"),
        ]
        desired.birthday = ContactBirthday(year: 1815, month: 12, day: 10)
        try ContactsConversion.apply(desired, photo: .keep, to: c)
        #expect(c.phoneNumbers.count == 3)
        #expect(c.phoneNumbers[0].identifier == before[0].identifier && c.phoneNumbers[0].label == CNLabelWork)
        #expect(c.phoneNumbers[1].identifier == before[1].identifier && c.phoneNumbers[1].value.stringValue == "+12025550199")
        #expect(!before.map(\.identifier).contains(c.phoneNumbers[2].identifier))
        #expect(c.phoneNumbers[2].label == CNLabelPhoneNumberiPhone)
        #expect(!c.phoneNumbers.map(\.identifier).contains(before[2].identifier))
        #expect(c.birthday?.year == 1815)
        #expect(c.emailAddresses.first?.label == CNLabelHome)
    }

    @Test func photoChangesAreExplicit() throws {
        let c = sample()
        let p = ContactsConversion.platform(c, containerId: nil)
        try ContactsConversion.apply(p, photo: .keep, to: c)
        #expect(c.imageData == Data([0xFF, 0xD8, 0xFF]))
        try ContactsConversion.apply(p, photo: .set(Data([1, 2, 3])), to: c)
        #expect(c.imageData == Data([1, 2, 3]))
        try ContactsConversion.apply(p, photo: .remove, to: c)
        #expect(c.imageData == nil)
        let oversized = Data(count: ContactsLimits.maxPhotoBytes + 1)
        #expect(throws: ContactsAdapterError.photoTooLarge(byteCount: oversized.count)) {
            try ContactsConversion.apply(p, photo: .set(oversized), to: c)
        }
    }

    @Test func scansNeverLoadFullImagesAndWritesFetchMutatedKeys() {
        let names = { (keys: [CNKeyDescriptor]) in Set(keys.compactMap { $0 as? String }) }
        #expect(!names(ContactsConversion.scanKeys).contains(CNContactImageDataKey))
        #expect(!names(ContactsConversion.scanKeys).contains(CNContactThumbnailImageDataKey))
        #expect(!names(ContactsConversion.scanKeys).contains(CNContactNoteKey))
        #expect(names(ContactsConversion.scanKeys).isSuperset(of: [
            CNContactPhoneNumbersKey, CNContactEmailAddressesKey, CNContactPostalAddressesKey, CNContactBirthdayKey,
        ]))
        #expect(!names(ContactsConversion.writeKeys(photo: .keep)).contains(CNContactImageDataKey))
        #expect(names(ContactsConversion.writeKeys(photo: .remove)).contains(CNContactImageDataKey))
        #expect(names(ContactsConversion.writeKeys(photo: .set(Data()))).contains(CNContactImageDataKey))
    }

    @Test func contactsErrorsMapToOutcomes() throws {
        #expect(try ContactsConversion.result(for: CNError(.recordDoesNotExist), access: .limited) == .outOfGrant)
        #expect(try ContactsConversion.result(for: CNError(.recordDoesNotExist), access: .full) == .notFound)
        #expect(try ContactsConversion.result(for: CNError(.authorizationDenied), access: .full) == .outOfGrant)
        #expect(try ContactsConversion.result(for: CNError(.recordNotWritable), access: .full) == .readOnly)
        #expect(try ContactsConversion.result(for: CNError(.parentContainerNotWritable), access: .full) == .readOnly)
        #expect(throws: CNError.self) { try ContactsConversion.result(for: CNError(.communicationError), access: .full) }
    }
}
#endif
