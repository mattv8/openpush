import SwiftUI
import UIKit

#if DEBUG
struct HostedOnboardingView: View {
    @State private var preview = HostedPreviewModel()
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.scenePhase) private var scenePhase
    @State private var showApproval = false
    @State private var showManageSubscription = false
    @State private var copied = false
    @State private var contactsEnabled = false
    @State private var actionTask: Task<Void, Never>?
    @State private var provisioningTask: Task<Void, Never>?
    let showSelfHosted: () -> Void

    private var colors: PeppyColorScheme { PeppyTokens.colors(for: colorScheme) }
    private func text(_ key: String) -> Text { Text(LocalizedStringKey("peppy." + key), tableName: "Peppy") }
    private func localized(_ key: String) -> LocalizedStringKey { LocalizedStringKey("peppy." + key) }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    text("preview_label").font(.footnote).foregroundStyle(colors.TextSecondary).accessibilityIdentifier("hosted-preview-label")
                    screen
                    status
                    Picker(selection: Binding(get: { preview.snapshot.scenario }, set: { preview.start(scenario: $0) })) {
                        ForEach(["new", "returning", "lapsed", "pending", "store_unavailable", "provision_retry", "approval_denied"], id: \.self) { Text(verbatim: $0).tag($0) }
                    } label: { text("preview_scenarios") }
                    .accessibilityIdentifier("preview-scenario-picker").disabled(preview.isWorking)
                    Button { preview.reset() } label: { text("preview_reset") }.accessibilityIdentifier("preview-reset-button").disabled(preview.isWorking)
                }.padding(24).frame(maxWidth: 560, alignment: .leading)
            }
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                if preview.snapshot.screen != "welcome" && preview.snapshot.screen != "settings" {
                    ToolbarItem(placement: .topBarLeading) { Button { cancelCurrentScreen() } label: { text("back") } }
                }
            }
            .sheet(isPresented: $showApproval, onDismiss: { if preview.snapshot.screen == "approval" { preview.advance("cancel") } }) { approvalSheet }
            .alert(Text(LocalizedStringKey("peppy.settings_server_manage"), tableName: "Peppy"), isPresented: $showManageSubscription) { Button { } label: { text("continue") } } message: { text("settings_server_manage_preview") }
        }
        .task { await preview.automaticWork() }
        .onChange(of: preview.snapshot.screen) { _, _ in
            copied = false
            if !preview.isWorking { run(preview.automaticWork) }
        }
        .onChange(of: preview.generatedPassphrase) { _, _ in phraseEdited() }
        .onChange(of: preview.customPassphrase) { _, _ in phraseEdited() }
        .onChange(of: scenePhase) { _, phase in
            if phase == .background {
                actionTask?.cancel(); provisioningTask?.cancel()
                showApproval = false; copied = false
                preview.resume()
            } else if phase == .active, !preview.isWorking { run(preview.automaticWork) }
        }
        .onDisappear { actionTask?.cancel(); provisioningTask?.cancel(); preview.cancelWork() }
    }

    @ViewBuilder private var screen: some View {
        switch preview.snapshot.screen {
        case "welcome":
            VStack(alignment: .leading, spacing: 16) {
                Image(decorative: "PeppyLogo").resizable().scaledToFit().frame(width: 88, height: 88)
                text("onboarding_headline").font(.largeTitle.bold()); text("onboarding_body")
                primary("onboarding_hosted_cta", id: "onboarding-hosted-cta") { preview.advance("hosted_start") }
                text("onboarding_hosted_sub").font(.footnote).foregroundStyle(colors.TextSecondary)
                Button(action: showSelfHosted) { text("onboarding_self_hosted_cta") }.accessibilityIdentifier("onboarding-self-hosted-cta")
            }.accessibilityIdentifier("onboarding-screen")
        case "signin": page("hosted-signin-screen", "hosted_sign_in_headline", "hosted_sign_in_body") {
            primary("hosted_sign_in_apple", id: "hosted-signin-apple") { run(preview.signIn) }
            primary("hosted_sign_in_google", id: "hosted-signin-google") { run(preview.signIn) }
        }
        case "subscribe": page("subscribe-screen", "hosted_subscribe_headline", "hosted_subscribe_body") {
            Text(verbatim: preview.price).foregroundStyle(colors.TextSecondary)
            if preview.storeUnavailable {
                Button { preview.advance("store_retry") } label: { text("try_again") }.accessibilityIdentifier("store-retry-button")
            } else {
                primary(preview.hasResumableEntitlement ? "continue" : "hosted_subscribe_cta", id: "subscribe-button") { run(preview.subscribe) }
                Button { run(preview.restore) } label: { text("hosted_subscribe_restore") }.accessibilityIdentifier("restore-purchases-button").disabled(preview.isWorking)
            }
            text("hosted_subscribe_legal").font(.footnote)
        }
        case "subscription_verifying": page("subscribe-screen", "hosted_purchase_verifying", nil) {
            if preview.localError == nil { ProgressView().accessibilityIdentifier("subscription-verifying-progress") }
            else { Button { run(preview.automaticWork) } label: { text("try_again") } }
        }
        case "purchase_pending": page("subscribe-screen", "hosted_purchase_pending", "hosted_purchase_pending_body") { Button { run(preview.restore) } label: { text("preview_continue") }.disabled(preview.isWorking) }
        case "passphrase": passphraseCreate
        case "confirm": passphraseConfirm
        case "provisioning": page("provisioning-screen", "provisioning_headline", nil) {
            if preview.snapshot.status_key == nil && preview.localError == nil {
                ProgressView().accessibilityIdentifier("provisioning-progress")
            } else {
                primary("try_again", id: "provisioning-retry-button") { run(preview.retryProvision) }
            }
        }
        case "join": page("hosted-join-screen", "hosted_join_headline", "hosted_join_body") {
            Text(verbatim: "PREVIEW-123").font(.title.monospaced())
            Button { preview.advance("show_approval"); showApproval = true } label: { text("preview_approval") }.accessibilityIdentifier("preview-approval-button").disabled(preview.isWorking)
            Button { preview.advance("passphrase_fallback") } label: { text("hosted_join_fallback") }.accessibilityIdentifier("passphrase-fallback-button").disabled(preview.isWorking)
        }
        case "approval": page("hosted-join-screen", "hosted_join_headline", "hosted_join_body") {
            Text(verbatim: "PREVIEW-123").font(.title.monospaced())
            Button { showApproval = true } label: { text("preview_approval") }.accessibilityIdentifier("preview-approval-button").disabled(preview.isWorking)
            Button { preview.advance("passphrase_fallback") } label: { text("hosted_join_fallback") }.accessibilityIdentifier("passphrase-fallback-button").disabled(preview.isWorking)
        }
        case "unlock": unlock
        case "lapsed": page("hosted-lapsed-screen", "hosted_lapsed_headline", "hosted_lapsed_body") {
            primary("hosted_lapsed_resubscribe", id: "resubscribe-button") { preview.advance("resubscribe") }
            Button { preview.advance("signout") } label: { text("settings_server_sign_out") }
        }
        case "permissions": page("permissions-screen", "permissions_headline", "permissions_body") {
            Toggle(isOn: $contactsEnabled) { text("permissions_contacts") }.accessibilityIdentifier("permissions-contacts-toggle").disabled(preview.isWorking)
            text("permissions_ios_note").font(.footnote)
            primary("permissions_done", id: "permissions-done-button") { preview.advance("permissions_done") }
            Button { preview.advance("permissions_skipped") } label: { text("permissions_skip") }.disabled(preview.isWorking)
        }
        case "settings": page("settings-server-section", "preview_complete", nil) {
            LabeledContent { Text(verbatim: "peppy.pro") } label: { text("settings_server_hosted") }
            LabeledContent { text(entitlementCopyKey) } label: { text("settings_server_status") }
            LabeledContent { Text(verbatim: "preview-account") } label: { text("settings_server_account") }
            if preview.snapshot.entitlement_state == "expired" || preview.snapshot.entitlement_state == "revoked" {
                text("hosted_lapsed_body")
                Button { preview.beginRenewal() } label: { text("hosted_lapsed_resubscribe") }.disabled(preview.isWorking)
            } else {
                Button { showManageSubscription = true } label: { text("settings_server_manage") }
            }
            Button { preview.advance("signout") } label: { text("settings_server_sign_out") }
            Button(role: .destructive) { preview.advance("delete_account") } label: { text("settings_server_delete") }
        }
        case "delete_account": page("settings-server-section", "settings_server_delete_title", "settings_server_delete_body") {
            Button(role: .destructive) { preview.advance("deletion_confirmed") } label: { text("settings_server_delete") }
            Button { preview.advance("cancel") } label: { text("back") }
        }
        default: EmptyView()
        }
    }

    private var entitlementCopyKey: String { switch preview.snapshot.entitlement_state { case "active": "settings_server_active"; case "grace": "settings_server_grace"; case "billing_retry": "settings_server_retry"; case "expired": "settings_server_expired"; case "revoked": "settings_server_revoked"; default: "settings_server_none" } }
    private var passphraseCreate: some View { page("passphrase-create-screen", "passphrase_create_headline", "passphrase_create_body") {
        text("passphrase_irrecoverable_warn").font(.footnote)
        if !preview.usesCustomPassphrase {
            passphraseField("passphrase_field", text: $preview.generatedPassphrase)
            Button { preview.generatePassphrase() } label: { text("passphrase_generate_cta") }.disabled(preview.isWorking)
            Button { copyPassphrase() } label: { text(copied ? "passphrase_copied" : "passphrase_copy") }.disabled(preview.isWorking)
            Button { preview.chooseCustomPassphrase() } label: { text("passphrase_custom_cta") }.disabled(preview.isWorking)
            text("passphrase_suggestion_note").font(.footnote).foregroundStyle(colors.TextSecondary)
        } else {
            passphraseField("passphrase_field", text: $preview.customPassphrase)
            Button { preview.generatePassphrase() } label: { text("passphrase_use_generated") }
        }
        Button { preview.revealPassphrase.toggle() } label: { text(preview.revealPassphrase ? "passphrase_hide" : "passphrase_show") }.disabled(preview.isWorking)
        Toggle(isOn: $preview.acknowledgement) { text("passphrase_ack_label") }.disabled(preview.isWorking)
        let value = preview.customPassphrase.isEmpty ? preview.generatedPassphrase : preview.customPassphrase
        let isValid = !value.isEmpty && hostedPreviewPassphraseAcceptable(passphrase: value)
        primary("passphrase_create_submit", id: "passphrase-create-button") { preview.acceptPassphrase() }.disabled(!isValid || !preview.acknowledgement)
    } }
    private var passphraseConfirm: some View { page("passphrase-confirm-screen", "passphrase_confirm_headline", "passphrase_confirm_body") { passphraseField("passphrase_confirm_field", text: $preview.confirmationPassphrase); primary("continue", id: "passphrase-confirm-button") { preview.confirmPassphrase() } } }
    private var unlock: some View { page("hosted-join-screen", "hosted_unlock_headline", "hosted_unlock_body") { passphraseField("passphrase_field", text: $preview.confirmationPassphrase); primary("hosted_unlock_cta", id: "hosted-unlock-button") { preview.confirmPassphrase(unlock: true) } } }
    private var approvalSheet: some View { VStack(spacing: 16) { text("device_approval_headline").font(.title2.bold()); text("device_approval_body"); Text(verbatim: "PREVIEW-123").font(.title.monospaced()); Button { preview.advance("approval_granted"); showApproval = false } label: { text("device_allow") }.accessibilityIdentifier("device-allow-button"); Button(role: .destructive) { preview.advance("approval_denied"); showApproval = false } label: { text("device_deny") }.accessibilityIdentifier("device-deny-button") }.padding().accessibilityIdentifier("device-approval-sheet").presentationDetents([.medium]).presentationDragIndicator(.visible) }
    @ViewBuilder private func page(_ id: String, _ title: String, _ body: String?, @ViewBuilder content: () -> some View) -> some View { VStack(alignment: .leading, spacing: 16) { text(title).font(.title.bold()); if let body { text(body) }; content() }.accessibilityIdentifier(id) }
    @ViewBuilder private var status: some View { if preview.isWorking { ProgressView().accessibilityIdentifier("hosted-preview-progress") }; if let key = preview.localError ?? preview.snapshot.status_key { text(key).foregroundStyle(colors.Error).accessibilityIdentifier("hosted-preview-error").accessibilityAddTraits(.isStaticText) } }
    private func primary(_ key: String, id: String, action: @escaping () -> Void) -> some View { Button(action: action) { text(key).frame(maxWidth: .infinity) }.buttonStyle(.borderedProminent).tint(colors.Accent).foregroundStyle(colors.AccentText).accessibilityIdentifier(id).disabled(preview.isWorking) }
    private func passphraseField(_ key: String, text binding: Binding<String>) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            text(key).font(.subheadline)
            if preview.revealPassphrase {
                TextField(text: binding) { text(key) }
            } else {
                SecureField(text: binding) { text(key) }
            }
        }
        .textContentType(.password)
        .textInputAutocapitalization(.never)
        .autocorrectionDisabled()
        .privacySensitive()
        .textFieldStyle(.roundedBorder)
        .accessibilityIdentifier("hosted-passphrase-field")
        .disabled(preview.isWorking)
    }
    private func copyPassphrase() { let value = preview.customPassphrase.isEmpty ? preview.generatedPassphrase : preview.customPassphrase; UIPasteboard.general.setItems([["public.utf8-plain-text": value]], options: [.localOnly: true, .expirationDate: Date().addingTimeInterval(60)]); copied = true }
    private func phraseEdited() { copied = false; preview.acknowledgement = false; preview.confirmationPassphrase = "" }
    private func cancelCurrentScreen() { actionTask?.cancel(); provisioningTask?.cancel(); preview.cancelWork(); preview.advance("back") }
    private func run(_ action: @escaping @MainActor () async -> Void) { actionTask?.cancel(); actionTask = Task { await action() } }
}
#endif
