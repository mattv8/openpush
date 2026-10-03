import SwiftUI
import UniformTypeIdentifiers
#if canImport(PeppyNative)
import PeppyNative
#endif

@main
struct PeppyMobileApp: App {
    @State private var model: AppModel

    init() {
        let model = AppModel()
        // BackgroundTasks must be registered before launch completes.
        model.registerBackgroundTasks()
        _model = State(initialValue: model)
    }

    var body: some Scene {
        WindowGroup {
            DeviceView(model: model)
        }
    }
}

/// One native form: status, credential import, vault unlock, foreground sync and carrier capability.
struct DeviceView: View {
    let model: AppModel
    @Environment(\.scenePhase) private var scenePhase
    @State private var passphrase = ""
    @State private var choosingFile = false
    @State private var confirmingDisconnect = false

    var body: some View {
        NavigationStack {
            Form {
                statusSection
                if model.status.identity == nil { importSection }
                if model.status.databaseOpen && !model.status.keysUnlocked { unlockSection }
                if model.status.keysUnlocked { syncSection }
                if model.status.databaseOpen { ContactsSection(model: model) }
                carrierSection
            }
            .navigationTitle("Peppy")
            .accessibilityIdentifier("device-form")
        }
        .task { await model.load() }
        // Runs one bounded pass each time the scene becomes active; cancelled when it leaves.
        .task(id: scenePhase == .active && model.status.keysUnlocked) {
            if scenePhase == .active { await model.syncInForeground() }
        }
        .onChange(of: scenePhase, initial: true) { _, phase in
            model.observeContactChanges(phase == .active)
            if phase == .background { model.scheduleContactsBackgroundWork() }
        }
        .fileImporter(isPresented: $choosingFile, allowedContentTypes: [.json]) { result in
            Task { await model.importCredential(from: result) }
        }
    }

    private var statusSection: some View {
        Section("Status") {
            if let identity = model.status.identity {
                LabeledContent("Server", value: identity.origin)
                LabeledContent("Vault", value: identity.vaultId)
                LabeledContent("Device", value: identity.deviceId)
                LabeledContent("Role", value: model.status.role ?? "unknown")
                LabeledContent("Local database", value: model.status.databaseOpen ? "Open (encrypted)" : "Closed")
                LabeledContent("Vault keys", value: model.status.keysUnlocked ? "Unlocked" : "Locked")
                if let count = model.status.conversations { LabeledContent("Conversations", value: "\(count)") }
                if model.status.rejectedKeyCaches > 0 {
                    Text("\(model.status.rejectedKeyCaches) stored key cache(s) were rejected; unlock with the vault passphrase.")
                }
            } else {
                Text("Not enrolled.")
            }
            if let error = model.lastError {
                Text(error).foregroundStyle(.red).accessibilityIdentifier("status-error")
            }
            if model.status.identity != nil {
                Button("Refresh from server") { Task { await model.refresh() } }
                    .disabled(model.activity != .idle)
                    .accessibilityIdentifier("refresh-enrollment-button")
                Button("Replace credential file…") { choosingFile = true }
                    .disabled(model.activity != .idle)
                    .accessibilityIdentifier("replace-credential-button")
            }
            if model.status.identity != nil || model.enrollmentBlocked {
                Button("Disconnect this device…") { confirmingDisconnect = true }
                    .disabled(model.activity != .idle)
                    .accessibilityIdentifier("disconnect-button")
            }
            if model.activity == .refreshing { ProgressView("Checking with server…") }
        }
        .accessibilityIdentifier("status-section")
        .confirmationDialog("Disconnect this device?", isPresented: $confirmingDisconnect, titleVisibility: .visible) {
            Button("Disconnect") { Task { await model.disconnect() } }
                .accessibilityIdentifier("confirm-disconnect-button")
        } message: {
            Text("Peppy stops using this enrollment. Its encrypted local data, keys and unsent messages stay on this device and come back if you import the same credential again. Nothing is deleted.")
        }
    }

    private var importSection: some View {
        Section {
            Button("Import credential file…") { choosingFile = true }
                .disabled(model.activity != .idle)
                .accessibilityIdentifier("import-credential-button")
            if model.activity == .importing { ProgressView("Verifying with server…") }
        } header: {
            Text("Import")
        } footer: {
            Text("Choose the device credential file created by operator or simulator pairing. The server is checked before anything is saved; the token is kept in this device's Keychain.")
        }
        .accessibilityIdentifier("import-section")
    }

    private var unlockSection: some View {
        Section {
            SecureField("Vault passphrase", text: $passphrase)
                .textContentType(.password)
                .accessibilityIdentifier("vault-passphrase-field")
            Button("Unlock") {
                let entered = passphrase
                passphrase = ""
                Task { await model.unlock(passphrase: entered) }
            }
            .disabled(passphrase.isEmpty || model.activity != .idle)
            .accessibilityIdentifier("unlock-button")
            if model.activity == .unlocking { ProgressView("Unlocking…") }
        } header: {
            Text("Unlock")
        } footer: {
            Text("Enter the vault's existing shared passphrase. It is not stored; only an opaque key cache is kept in the Keychain on this device.")
        }
        .accessibilityIdentifier("unlock-section")
    }

    private var syncSection: some View {
        Section {
            if model.activity == .syncing { ProgressView("Syncing…") }
            if let report = model.lastSync, let date = model.lastSyncDate {
                LabeledContent("Last pass", value: date.formatted(date: .omitted, time: .standard))
                LabeledContent("Sent / received", value: "\(report.uploaded) / \(report.journaled)")
                LabeledContent("Applied", value: "\(report.applied)")
                if report.waitingForKeys > 0 { LabeledContent("Waiting for keys", value: "\(report.waitingForKeys)") }
                if report.snapshotPublished { Text("Resynced from a server snapshot.") }
                if !report.complete { Text("More work remains; it continues on the next foreground pass.") }
            }
        } header: {
            Text("Sync")
        } footer: {
            Text("Messages sync while Peppy is open. Contacts may also update in the background when iOS allows it; there is no guaranteed timing.")
        }
        .accessibilityIdentifier("sync-section")
    }

    private var carrierSection: some View {
        Section {
            LabeledContent("iOS carrier messaging", value: model.telephony.canExecuteCarrierCommands ? "Available" : "Unavailable")
            ForEach(model.telephony.blockers, id: \.explanation) { blocker in
                Text(blocker.explanation)
            }
            if let pending = model.lastSync?.carrierCommandsNotExecuted, pending > 0 {
                Text("\(pending) send request(s) for this device are queued and will not be sent from iOS.")
            }
        } header: {
            Text("Carrier messaging")
        } footer: {
            Text("SMS/MMS sending and receiving is done by an Android gateway with the default SMS role. iOS cannot act as a gateway in this build. RCS is not supported. Carrier messages are not end-to-end encrypted; sync between your devices is.")
        }
        .accessibilityIdentifier("carrier-section")
    }
}
