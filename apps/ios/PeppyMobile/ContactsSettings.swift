import Foundation
import SwiftUI
#if canImport(Contacts)
import Contacts
#endif
#if canImport(UIKit)
import UIKit
#endif
#if canImport(PeppyNative)
import PeppyNative
#endif

/// UI state for contact sync. Domain state comes from the core via `ContactsOverview`.
struct ContactsState {
    var enabled = false
    var access: ContactsAccess = .notDetermined
    var overview = ContactsOverview()
    var lastReport: ContactsSyncReport?
    var lastPassDate: Date?
    var working = false
    var error: String?
}

extension AppModel {
    /// Reads access, containers and the owner book without touching contacts.
    func refreshContacts() async {
        contacts.access = (try? await contactsProvider.access()) ?? .notDetermined
        if status.databaseOpen { contacts.overview = (try? await session.contactsOverview()) ?? contacts.overview }
    }

    /// Explicit user action only: asks for Contacts access in the foreground, then opts in.
    func enableContacts() async {
        contacts.working = true
        defer { contacts.working = false }
        #if canImport(Contacts)
        if (try? await contactsProvider.access()) == .notDetermined {
            _ = try? await CNContactStore().requestAccess(for: .contacts)
        }
        #endif
        var preferences = contactsPreferences.load()
        preferences.enabled = true
        contactsPreferences.save(preferences)
        contacts.enabled = true
        scheduleContactsBackgroundWork()
        await refreshContacts()
        await runContactsPass(.foreground)
    }

    /// Stops syncing without publishing anything; the book stays as last published.
    func pauseContacts() {
        var preferences = contactsPreferences.load()
        preferences.enabled = false
        contactsPreferences.save(preferences)
        contacts.enabled = false
    }

    /// The new-contact account is an owner setting in the core, not a phone preference.
    func setDefaultAccount(_ id: String) async {
        await contactsAction { try await $0.setContactsDefaultAccount(id) }
    }

    func setRemoteEdits(_ mode: String) async {
        await contactsAction { try await $0.setContactsRemoteEdits(mode) }
    }

    /// Approval is recorded locally; the next pass takes the one-use permit and writes.
    func decideContactRequest(_ request: ContactsPendingRequest, approve: Bool) async {
        await contactsAction { try await $0.decideContactRequest(request.id, approve: approve, scanHold: request.isScanHold) }
        if approve { await runContactsPass(.foreground) }
    }

    func retireContacts() async {
        let preferences = contactsPreferences
        await contactsAction { try await $0.retireContacts(preferences: preferences) }
        contacts.enabled = contactsPreferences.load().enabled
        contacts.lastReport = nil
    }

    func runContactsPass(_ reason: ContactsPassReason) async {
        guard contacts.enabled, status.databaseOpen else { return }
        do {
            try await contactPasses.run(reason)
        } catch ClientError.canceled {
        } catch is CancellationError {
        } catch {
            contacts.error = (error as? ClientError)?.userMessage ?? "Contact sync failed."
        }
    }

    func contactsPassFinished(_ report: ContactsSyncReport) async {
        contacts.lastReport = report
        if report.outcome != .busy && report.outcome != .disabled { contacts.lastPassDate = Date() }
        contacts.error = report.syncError
        await refreshContacts()
    }

    /// Foreground-only change observation; background freshness comes from BackgroundTasks.
    func observeContactChanges(_ active: Bool) {
        #if canImport(Contacts)
        if active, contactsObserver == nil {
            contactsObserver = NotificationCenter.default.addObserver(
                forName: .CNContactStoreDidChange, object: nil, queue: .main
            ) { [weak self] _ in
                Task { @MainActor in await self?.runContactsPass(.changeNotification) }
            }
        } else if !active, let observer = contactsObserver {
            NotificationCenter.default.removeObserver(observer)
            contactsObserver = nil
        }
        #endif
    }

    private func contactsAction(_ body: (NativeSession) async throws -> ContactsOverview) async {
        contacts.working = true
        defer { contacts.working = false }
        do {
            contacts.overview = try await body(session)
            contacts.error = nil
        } catch {
            contacts.error = (error as? ClientError)?.userMessage ?? "The contact setting could not be saved."
        }
    }
}

/// Contacts settings: opt-in, access, new-contact container, remote edit policy, last pass,
/// pending approvals and retire. Never promises a sync latency.
struct ContactsSection: View {
    let model: AppModel
    @State private var confirmingRetire = false

    var body: some View {
        Section {
            if !model.contacts.enabled {
                Button("Sync this phone's contacts…") { Task { await model.enableContacts() } }
                    .disabled(model.contacts.working || !model.status.keysUnlocked)
                    .accessibilityIdentifier("contacts-enable-button")
            } else {
                accessRows
                containerPicker
                policyPicker
                statusRows
                approvals
                Button("Pause contact sync") { model.pauseContacts() }
                    .accessibilityIdentifier("contacts-pause-button")
                Button("Retire this phone's contact book…", role: .destructive) { confirmingRetire = true }
                    .disabled(model.contacts.working || model.contacts.overview.bookId == nil)
                    .accessibilityIdentifier("contacts-retire-button")
            }
            if let error = model.contacts.error {
                Text(error).foregroundStyle(.red).accessibilityIdentifier("contacts-error")
            }
        } header: {
            Text("Contacts")
        } footer: {
            Text("This phone publishes its own contact book to your vault and applies edits from your other devices. iOS decides when background updates run; it can take hours or days. Notes are not synced from iOS.")
        }
        .accessibilityIdentifier("contacts-section")
        .task(id: model.status.databaseOpen) { await model.refreshContacts() }
        .confirmationDialog("Retire this contact book?", isPresented: $confirmingRetire, titleVisibility: .visible) {
            Button("Retire", role: .destructive) { Task { await model.retireContacts() } }
                .accessibilityIdentifier("contacts-confirm-retire-button")
        } message: {
            Text("Your other devices will show this book as retired. Contacts on this phone are not changed or deleted. Turning sync on again starts a new book.")
        }
    }

    @ViewBuilder private var accessRows: some View {
        LabeledContent("Access", value: Self.accessText(model.contacts.access))
            .accessibilityIdentifier("contacts-access-row")
        switch model.contacts.access {
        case .limited, .denied, .restricted:
            Text(model.contacts.access == .limited
                 ? "Only the contacts you selected are synced. Contacts outside the selection are never treated as deleted."
                 : "Contacts are unavailable. Your other devices keep the last synced book and see it as unavailable.")
            #if canImport(UIKit)
            Button("Manage access in Settings") {
                if let url = URL(string: UIApplication.openSettingsURLString) { UIApplication.shared.open(url) }
            }
            .accessibilityIdentifier("contacts-manage-access-button")
            #endif
        case .notDetermined:
            Button("Allow Contacts access…") { Task { await model.enableContacts() } }
                .accessibilityIdentifier("contacts-request-access-button")
        case .full:
            EmptyView()
        }
    }

    @ViewBuilder private var containerPicker: some View {
        let accounts = model.contacts.overview.accounts.filter(\.writable)
        if !accounts.isEmpty {
            Picker("Save new contacts to", selection: Binding(
                get: { model.contacts.overview.defaultAccountId ?? accounts.first?.id ?? "" },
                set: { id in if !id.isEmpty { Task { await model.setDefaultAccount(id) } } }
            )) {
                ForEach(accounts) { account in
                    Text(account.name).tag(account.id)
                }
            }
            .accessibilityIdentifier("contacts-container-picker")
        }
    }

    @ViewBuilder private var policyPicker: some View {
        if model.contacts.overview.bookId != nil {
            Picker("Edits from other devices", selection: Binding(
                get: { model.contacts.overview.remoteEdits ?? "confirm" },
                set: { mode in Task { await model.setRemoteEdits(mode) } }
            )) {
                Text("Apply automatically").tag("auto")
                Text("Ask me first").tag("confirm")
                Text("Off").tag("off")
            }
            .accessibilityIdentifier("contacts-remote-edits-picker")
        }
    }

    @ViewBuilder private var statusRows: some View {
        if let count = model.contacts.overview.contactCount {
            LabeledContent("Contacts in book", value: "\(count)")
        }
        if let date = model.contacts.lastPassDate {
            LabeledContent("Last checked", value: date.formatted(.relative(presentation: .named)))
                .accessibilityIdentifier("contacts-last-pass-row")
        } else {
            Text("Not checked yet in this session.")
        }
        if let report = model.contacts.lastReport {
            if let text = Self.outcomeText(report) { Text(text).accessibilityIdentifier("contacts-outcome-text") }
            if report.held > 0 {
                Text("\(report.held) contacts disappeared at once; removing them from your other devices is on hold.")
            }
            if report.limitExceeded { Text("This book exceeds 20,000 contacts; only part of it is synced.") }
            if report.photoFailures > 0 {
                Text("\(report.photoFailures) contact photo(s) could not be prepared.")
                    .accessibilityIdentifier("contacts-photo-failures")
            }
            if report.media.unavailable > 0 {
                Text("\(report.media.unavailable) contact photo(s) are unavailable on the server and will not be retried immediately.")
                    .accessibilityIdentifier("contacts-photo-unavailable")
            }
            if report.media.failed > 0 {
                Text("\(report.media.failed) contact photo transfer(s) failed; they will be retried later.")
                    .accessibilityIdentifier("contacts-photo-transfer-failures")
            }
            if report.retentionPaused {
                Text("Contact photo retention is paused until all vault devices support active compaction.")
                    .accessibilityIdentifier("contacts-retention-paused")
            }
        }
        if model.contacts.working { ProgressView() }
    }

    @ViewBuilder private var approvals: some View {
        let pending = model.contacts.overview.pending
        if !pending.isEmpty {
            ForEach(pending) { request in
                VStack(alignment: .leading) {
                    Text(Self.requestText(request))
                    if let detail = Self.requestDetail(request) { Text(detail).font(.footnote).foregroundStyle(.secondary) }
                    if request.state == "requested" || request.state == "awaiting_approval" {
                        HStack {
                            Button("Approve") { Task { await model.decideContactRequest(request, approve: true) } }
                                .accessibilityIdentifier("contacts-approve-\(request.id)")
                            Button("Reject", role: .destructive) { Task { await model.decideContactRequest(request, approve: false) } }
                                .accessibilityIdentifier("contacts-reject-\(request.id)")
                        }
                        .buttonStyle(.borderless)
                    }
                }
                .accessibilityElement(children: .contain)
                .accessibilityIdentifier("contacts-request-\(request.id)")
            }
        }
    }

    static func accessText(_ access: ContactsAccess) -> String {
        switch access {
        case .full: "All contacts"
        case .limited: "Selected contacts"
        case .denied: "Denied"
        case .restricted: "Restricted"
        case .notDetermined: "Not requested"
        }
    }

    static func outcomeText(_ report: ContactsSyncReport) -> String? {
        switch report.outcome {
        case .needsUnlock: "Waiting for the vault to be unlocked on this phone."
        case .busy: "Another sync was running; contacts are checked next."
        case .serverIncompatible: "This server cannot safely compact contact history; contacts are paused."
        case .backfillPending: "Preparing existing contact history for safe sync; contacts resume on a later check."
        case .noAccess: "Contacts access is off; nothing was removed from your other devices."
        case .partial: "The last check was interrupted or partial; nothing was removed. It continues next time."
        case .repairPending: report.projectionFailure.map { "Rebuilding contacts from the server failed (\($0)); it is retried on the next check." }
            ?? "Contacts are being rebuilt from the server before this phone publishes changes."
        case .unchanged, .incremental, .completed, .disabled:
            report.photoWorkRemaining ? "Contact photos are still being checked; this continues on later checks." : nil
        }
    }

    static func requestText(_ request: ContactsPendingRequest) -> String {
        if request.isScanHold {
            return "\(request.count ?? 0) contacts disappeared at once. Remove them from your other devices?"
        }
        let name = request.displayName ?? "a contact"
        switch request.state {
        case "requested", "awaiting_approval":
            return switch request.kind {
            case "create": "Another device wants to add \(name)"
            case "delete": "Another device wants to delete \(name)"
            default: "Another device wants to edit \(name)"
            }
        case "approved": return "Approved; applied on the next contact check"
        case "applying", "outcome_unknown": return "An edit to \(name) may or may not have been saved; check this contact. It will not be retried."
        default: return request.state
        }
    }

    static func requestDetail(_ request: ContactsPendingRequest) -> String? {
        if request.isScanHold { return request.sampleNames.isEmpty ? nil : request.sampleNames.joined(separator: ", ") }
        return request.fieldPaths.isEmpty ? nil : "Fields: " + request.fieldPaths.joined(separator: ", ")
    }
}
