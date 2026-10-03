import SwiftUI
import UniformTypeIdentifiers
#if canImport(PeppyNative)
import PeppyNative
#endif

@main
struct PeppyMobileApp: App {
    @State private var model: AppModel
    @UIApplicationDelegateAdaptor(PushWakeDelegate.self) private var pushDelegate

    init() {
        let model = AppModel()
        model.registerBackgroundTasks()
        PushWakeDelegate.install(model: model)
        _model = State(initialValue: model)
    }

    var body: some Scene {
        WindowGroup { DeviceView(model: model) }
    }
}

/// The native gateway shell deliberately owns presentation only. Enrollment, encrypted state,
/// contacts and bounded synchronization remain in `NativeSession`/the Rust core.
struct DeviceView: View {
    let model: AppModel
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.colorScheme) private var colorScheme
    @State private var passphrase = ""
    @State private var choosingFile = false
    @State private var showingPairing = false
    @State private var showingSettings = false
    @State private var confirmingDisconnect = false
    @State private var devicePendingRemoval: DeviceRosterItem?
    @State private var vaultDeleteText = ""
    @State private var showingVaultDelete = false

    var body: some View {
        Group {
            if model.status.identity == nil {
                welcome
            } else if model.status.databaseOpen && !model.status.keysUnlocked {
                lockScreen
            } else {
                enrolledTabs
            }
        }
        .tint(colors.Accent)
        .task { await model.load() }
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
        .sheet(isPresented: $showingPairing) { PairingView(model: model, isPresented: $showingPairing) }
    }

    private var colors: PeppyColorScheme { PeppyTokens.colors(for: colorScheme) }

    private var welcome: some View {
        PeppyGlassSurface(colors: colors) {
            VStack(spacing: 16) {
                Image(systemName: "lock.shield")
                    .font(.system(size: 48)).foregroundStyle(colors.Accent)
                Text("Pair this phone with your Peppy vault").font(.title2).bold()
                Button("Scan QR code") { showingPairing = true }
                    .buttonStyle(.borderedProminent).accessibilityIdentifier("pair-qr-button")
                Button("Import credential file…") { choosingFile = true }
                    .accessibilityIdentifier("import-credential-button")
                activityAndError
            }
            .padding(24)
        }
        .frame(maxWidth: 460)
        .accessibilityIdentifier("welcome-screen")
    }

    private var lockScreen: some View {
        PeppyGlassSurface(colors: colors) {
            VStack(spacing: 16) {
                Image(systemName: "lock.fill").font(.system(size: 42)).foregroundStyle(colors.Accent)
                Text("Vault locked").font(.title2).bold()
                if let identity = model.status.identity {
                    LabeledContent("Server", value: identity.origin)
                    LabeledContent("Device", value: identity.deviceId)
                }
                SecureField("Vault passphrase", text: $passphrase)
                    .textContentType(.password).textFieldStyle(.roundedBorder)
                    .accessibilityIdentifier("vault-passphrase-field")
                Button("Unlock") {
                    let value = passphrase; passphrase = ""
                    Task { await model.unlock(passphrase: value) }
                }.disabled(passphrase.isEmpty || model.activity != .idle)
                    .accessibilityIdentifier("unlock-button")
                if model.enrollmentBlocked {
                    Button("Replace credential file…") { choosingFile = true }
                        .accessibilityIdentifier("replace-credential-button")
                }
                activityAndError
            }
            .padding(24)
        }
        .frame(maxWidth: 460)
        .accessibilityIdentifier("lock-screen")
    }

    private var enrolledTabs: some View {
        TabView {
            mirroringTab.tabItem { Label { Text("peppy.mirroring", tableName: "Peppy") } icon: { Image(systemName: "app.badge") } }
                .accessibilityIdentifier("tab-mirroring")
            smsTab.tabItem { Label { Text("peppy.sms", tableName: "Peppy") } icon: { Image(systemName: "message") } }
                .accessibilityIdentifier("tab-sms")
            accountTab.tabItem { Label { Text("peppy.account", tableName: "Peppy") } icon: { Image(systemName: "person.crop.circle") } }
                .accessibilityIdentifier("tab-account")
        }
        .sheet(isPresented: $showingSettings) { SettingsView(model: model) }
    }

    private var mirroringTab: some View {
        NavigationStack {
            Form {
                Section("Mirroring") {
                    LabeledContent("Notification mirroring", value: "Unavailable on iOS")
                        .accessibilityValue("Unavailable")
                        .accessibilityIdentifier("mirroring-ios-unavailable-row")
                    Text("iOS does not provide a system-wide notification listener API. Mirroring is available on an Android gateway.")
                        .foregroundStyle(.secondary)
                }.accessibilityIdentifier("mirroring-ios-section")
            }.formStyle(.grouped).navigationTitle(Text("peppy.mirroring", tableName: "Peppy")).toolbar { settingsButton }
        }
    }

    private var smsTab: some View {
        NavigationStack {
            Form {
                syncSection
                Section {
                    LabeledContent("iOS carrier messaging", value: "Unavailable")
                        .accessibilityValue("Unavailable").accessibilityIdentifier("carrier-status-row")
                    ForEach(Array(model.telephony.blockers.enumerated()), id: \.offset) { index, blocker in
                        Text(blocker.explanation).accessibilityIdentifier("carrier-blocker-\(index)")
                    }
                    if let pending = model.lastSync?.carrierCommandsNotExecuted, pending > 0 {
                        Text("\(pending) send request(s) for this device are queued and will not be sent from iOS.")
                            .accessibilityIdentifier("carrier-pending-commands")
                    }
                } header: {
                    Text("Carrier messaging")
                } footer: {
                    Text("iOS cannot capture or send carrier SMS/MMS. Use an Android gateway for carrier messaging. RCS is unavailable.")
                }.accessibilityIdentifier("carrier-section")
                Section("SMS and MMS") {
                    LabeledContent("SMS mirroring", value: "Unavailable on iOS")
                        .accessibilityValue("Unavailable").accessibilityIdentifier("sms-mirroring-unavailable-row")
                    Text("iOS does not provide an SMS inbox listener. An Android gateway receives and sends SMS.")
                    LabeledContent("MMS capture", value: "Unavailable on iOS")
                        .accessibilityValue("Unavailable").accessibilityIdentifier("mms-unavailable-row")
                    Text("MMS capture requires a carrier listener not available in this iOS build.")
                }
            }.formStyle(.grouped).navigationTitle(Text("peppy.sms", tableName: "Peppy")).toolbar { settingsButton }
        }.accessibilityIdentifier("sms-tab-screen")
    }

    private var syncSection: some View {
        Section("Sync") {
            if model.activity == .syncing { ProgressView("Syncing…") }
            if let report = model.lastSync, let date = model.lastSyncDate {
                LabeledContent("Last pass", value: date.formatted(date: .omitted, time: .standard))
                LabeledContent("Sent / received", value: "\(report.uploaded) / \(report.journaled)")
                LabeledContent("Applied", value: "\(report.applied)")
                if report.waitingForKeys > 0 { LabeledContent("Waiting for keys", value: "\(report.waitingForKeys)") }
                if !report.complete { Text("More work remains; it continues on the next foreground pass.") }
            } else { Text("Waiting for the next bounded foreground pass.") }
            Text("peppy.encrypted", tableName: "Peppy").font(.footnote).foregroundStyle(.secondary)
        }.accessibilityIdentifier("sync-section")
    }

    private var accountTab: some View {
        NavigationStack {
            Form {
                Section("Identity") {
                    if let identity = model.status.identity {
                        LabeledContent("Server", value: identity.origin).accessibilityIdentifier("status-server")
                        LabeledContent("Vault", value: identity.vaultId).accessibilityIdentifier("status-vault-id")
                        LabeledContent("Device", value: identity.deviceId).accessibilityIdentifier("status-device-id")
                    }
                    LabeledContent("Role", value: model.status.role ?? "unknown").accessibilityIdentifier("status-role")
                    LabeledContent("Local database", value: model.status.databaseOpen ? "Open (encrypted)" : "Closed").accessibilityIdentifier("status-database")
                    LabeledContent("Vault keys", value: model.status.keysUnlocked ? "Unlocked" : "Locked").accessibilityIdentifier("status-keys")
                    if let count = model.status.conversations { LabeledContent("Conversations", value: "\(count)").accessibilityIdentifier("status-conversations") }
                }.accessibilityIdentifier("status-section")
                Section("Session") {
                    Button("Refresh from server") { Task { await model.refresh() } }
                        .disabled(model.activity != .idle).accessibilityIdentifier("refresh-enrollment-button")
                    Button("Replace credential file…") { choosingFile = true }.accessibilityIdentifier("replace-credential-button")
                    Button("Disconnect this device…", role: .destructive) { confirmingDisconnect = true }
                        .disabled(model.activity != .idle).accessibilityIdentifier("disconnect-button")
                }
                devicesSection
                relaySection
                if model.status.role == "owner" { vaultSection }
                if let error = model.lastError { Text(error).foregroundStyle(colors.Error).accessibilityIdentifier("status-error") }
            }
            .formStyle(.grouped).navigationTitle(Text("peppy.account", tableName: "Peppy")).toolbar { settingsButton }
            .confirmationDialog("Disconnect this device?", isPresented: $confirmingDisconnect, titleVisibility: .visible) {
                Button("Disconnect", role: .destructive) { Task { await model.disconnect() } }
                    .accessibilityIdentifier("confirm-disconnect-button")
                Button("Keep connected", role: .cancel) {}
            } message: {
                Text("Disconnecting revokes this device’s server access and stops syncing. Encrypted local data stays archived on this device. To connect again, pair as a new device.")
            }
            .confirmationDialog("Remove this device?", isPresented: Binding(get: { devicePendingRemoval != nil }, set: { if !$0 { devicePendingRemoval = nil } }), titleVisibility: .visible) {
                Button("Remove device", role: .destructive) {
                    if let devicePendingRemoval { Task { await model.revokeDevice(devicePendingRemoval.id) } }
                    devicePendingRemoval = nil
                }.accessibilityIdentifier("confirm-device-remove-button")
                Button(role: .cancel) { devicePendingRemoval = nil } label: { Text("peppy.cancel", tableName: "Peppy") }
            } message: {
                Text("This revokes the selected device's server credential and access.")
            }
            .sheet(isPresented: $showingVaultDelete) {
                NavigationStack {
                    Form {
                        Section("Type ERASE to confirm") {
                            TextField("Type ERASE", text: $vaultDeleteText).autocorrectionDisabled()
                                .accessibilityIdentifier("vault-delete-confirm-field")
                            Button("Delete vault", role: .destructive) {
                                Task { await model.deleteVault(); showingVaultDelete = false }
                            }.disabled(vaultDeleteText != "ERASE" || model.activity != .idle)
                                .accessibilityIdentifier("vault-delete-final-button")
                        }
                    }.navigationTitle("Delete vault?")
                        .toolbar { ToolbarItem(placement: .topBarLeading) { Button("Cancel") { showingVaultDelete = false } } }
                }
            }
        }.accessibilityIdentifier("account-tab-screen")
    }

    private var devicesSection: some View {
        Section {
            if !model.devicesLoaded { ProgressView("Loading devices…").accessibilityIdentifier("devices-loading") }
            if model.devicesLoaded && model.devices.isEmpty { Text("No devices found.").accessibilityIdentifier("devices-empty") }
            ForEach(model.devices) { device in
                VStack(alignment: .leading) {
                    Text(device.id + (device.id == model.status.identity?.deviceId ? " (this device)" : ""))
                    Text(device.revoked ? "Revoked" : device.role).font(.footnote).foregroundStyle(.secondary)
                    if device.id == model.status.identity?.deviceId {
                        Button("Sign out", role: .destructive) { confirmingDisconnect = true }
                    } else if model.status.role == "owner" && !device.revoked {
                        Button("Remove device", role: .destructive) { devicePendingRemoval = device }
                            .accessibilityIdentifier("device-remove-\(device.id)")
                    }
                }.accessibilityIdentifier("device-row-\(device.id)")
            }
        } header: {
            Text("peppy.devices", tableName: "Peppy")
        }.accessibilityIdentifier("devices-section").task { await model.refreshDevices() }
    }

    private var relaySection: some View {
        Section("Wake relay") {
            LabeledContent("APNs", value: model.relay == nil ? "Not configured" : "Ready to register")
                .accessibilityIdentifier("relay-apns-row")
            Text(model.relay == nil ? "No relay configured. Sync runs while the app is open." : "Enable APNs registration from Settings. Delivery is not guaranteed.")
                .accessibilityIdentifier("relay-not-configured")
            Text("Wake hints are optional. Missing a hint delays sync briefly; no messages are lost.")
                .font(.footnote).foregroundStyle(.secondary)
        }.accessibilityIdentifier("relay-health-section")
    }

    private var vaultSection: some View {
        Section("Vault") {
            Button("Delete vault…", role: .destructive) { showingVaultDelete = true }
                .accessibilityIdentifier("vault-delete-button")
            Text("Vault deletion permanently removes server data and device access. Local encrypted data remains until uninstalled.")
                .font(.footnote).foregroundStyle(.secondary)
        }.accessibilityIdentifier("vault-section")
    }

    @ToolbarContentBuilder private var settingsButton: some ToolbarContent {
        ToolbarItem(placement: .topBarTrailing) {
            PeppyGlassSurface(colors: colors, cornerRadius: 16) {
                Button { showingSettings = true } label: { Label { Text("peppy.settings", tableName: "Peppy") } icon: { Image(systemName: "gearshape") } }
                    .padding(4)
            }
        }
    }

    @ViewBuilder private var activityAndError: some View {
        if model.activity != .idle { ProgressView("Verifying with server…").accessibilityIdentifier("enroll-progress") }
        if let error = model.lastError { Text(error).foregroundStyle(colors.Error).accessibilityIdentifier("enroll-error") }
    }
}

struct SettingsView: View {
    @Bindable var model: AppModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Form {
                Section("Gateway settings") {
                    LabeledContent("SMS capture", value: "Unavailable on iOS").accessibilityValue("Unavailable")
                        .accessibilityIdentifier("settings-sms-capture-toggle")
                    LabeledContent("MMS capture", value: "Unavailable on iOS").accessibilityValue("Unavailable")
                        .accessibilityIdentifier("settings-mms-capture-toggle")
                }
                Section("Mirroring settings") {
                    LabeledContent("Notification mirroring", value: "Unavailable on iOS").accessibilityValue("Unavailable")
                        .accessibilityIdentifier("settings-mirroring-toggle")
                }
                Section("Wake relay") {
                    TextField("Operator relay HTTPS origin", text: $model.relayOrigin)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                        .accessibilityIdentifier("relay-origin-field")
                    Button("Enable APNs wake relay") { model.configureRelay() }
                        .disabled(model.relayOrigin.isEmpty || model.status.identity == nil)
                        .accessibilityIdentifier("relay-enable-button")
                    Text("Peppy registers with the operator relay only after you enable this option. Wake hints contain no message content and may be delayed or missing.")
                        .font(.footnote).foregroundStyle(.secondary)
                }.accessibilityIdentifier("relay-settings-section")
                ContactsSection(model: model)
                Section("About") {
                    LabeledContent("App version", value: "0.1")
                    if let origin = model.status.identity?.origin { LabeledContent("Server", value: origin) }
                }
            }.formStyle(.grouped).navigationTitle(Text("peppy.settings", tableName: "Peppy"))
                .toolbar { ToolbarItem(placement: .topBarLeading) { Button("Done") { dismiss() } } }
        }.accessibilityIdentifier("settings-screen")
    }
}
