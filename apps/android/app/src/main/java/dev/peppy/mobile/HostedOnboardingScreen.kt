package dev.peppy.mobile

import android.content.ClipData
import android.os.PersistableBundle
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

@OptIn(androidx.compose.ui.ExperimentalComposeUiApi::class)
@Composable
internal fun HostedOnboardingScreen(onSelfHosted: () -> Unit, injectedModel: HostedPreviewModel? = null) {
    val context = LocalContext.current
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    val model = injectedModel ?: remember { HostedPreviewModel(context) }
    val scope = rememberCoroutineScope()
    var active by remember { mutableStateOf(lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED)) }
    var custom by remember { mutableStateOf("") }
    var confirmation by remember { mutableStateOf("") }
    var acknowledged by remember { mutableStateOf(false) }
    var revealed by remember { mutableStateOf(true) }
    var customMode by remember { mutableStateOf(false) }
    var errorKey by remember { mutableStateOf<String?>(null) }

    fun clearSecrets() {
        model.clearSecret(); custom = ""; confirmation = ""
        acknowledged = false; revealed = false; customMode = false; errorKey = null
    }
    DisposableEffect(lifecycle) {
        val observer = LifecycleEventObserver { _, event ->
            when (event) {
                Lifecycle.Event.ON_START -> active = true
                Lifecycle.Event.ON_STOP -> { active = false; model.onBackground(); clearSecrets() }
                else -> Unit
            }
        }
        lifecycle.addObserver(observer)
        onDispose { lifecycle.removeObserver(observer); model.onBackground(); clearSecrets() }
    }
    LaunchedEffect(active) { if (active) model.onForeground() }
    LaunchedEffect(active, model) {
        if (!active) return@LaunchedEffect
        snapshotFlow { Triple(model.screen, model.statusKey, model.isBusy) }.collect { (screen, status, busy) ->
            when {
                screen == "subscription_verifying" && model.localError == null && !busy -> {
                    delay(150)
                    model.verifyEntitlement()
                    if (model.screen == "passphrase") revealed = true
                }
                screen == "syncing" && status == null && !busy -> model.prepare()
                screen == "provisioning" && status == null && !busy -> {
                    delay(250)
                    model.provision()
                }
            }
        }
    }
    BackHandler(enabled = model.screen != "welcome") {
        if (model.screen == "checking_account") { clearSecrets(); model.advance("signout") } else { clearSecrets(); model.advance("back") }
    }
    val tag = when (model.screen) {
        "welcome" -> "onboarding-screen"
        "signin" -> "hosted-signin-screen"
        "checking_account" -> "checking-account-screen"
        "subscribe", "subscription_verifying", "purchase_pending" -> "subscribe-screen"
        "passphrase" -> "passphrase-create-screen"
        "confirm" -> "passphrase-confirm-screen"
        "provisioning" -> "provisioning-screen"
        "join", "approval" -> "hosted-join-screen"
        "unlock" -> "hosted-unlock-screen"
        "syncing" -> "syncing-screen"
        "lapsed" -> "hosted-lapsed-screen"
        "permissions" -> "permissions-screen"
        "delete_account" -> "delete-account-screen"
        else -> "settings-server-section"
    }
    Column(
        Modifier.fillMaxSize().safeDrawingPadding().imePadding().verticalScroll(rememberScrollState())
            .padding(24.dp).semantics { testTagsAsResourceId = true }.testTag(tag),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Text(stringResource(R.string.peppy_preview_label), style = MaterialTheme.typography.bodySmall)
        Image(painterResource(R.drawable.peppy_logo), null, Modifier.size(88.dp))
        when (model.screen) {
            "welcome" -> {
                Heading(R.string.peppy_onboarding_headline)
                Text(stringResource(R.string.peppy_onboarding_body))
                Action(R.string.peppy_onboarding_hosted_cta, "onboarding-hosted-cta") { model.advance("hosted_start") }
                Text(stringResource(R.string.peppy_onboarding_hosted_sub), style = MaterialTheme.typography.bodySmall)
                OutlinedButton({ clearSecrets(); onSelfHosted() }, Modifier.fillMaxWidth().testTag("onboarding-self-hosted-cta")) { Text(stringResource(R.string.peppy_onboarding_self_hosted_cta)) }
            }
            "signin" -> {
                Heading(R.string.peppy_hosted_sign_in_headline)
                Text(stringResource(R.string.peppy_hosted_sign_in_body))
                Action(R.string.peppy_hosted_sign_in_apple, "hosted-signin-apple", enabled = !model.isBusy) { scope.launch { model.signIn("apple") } }
                OutlinedButton({ scope.launch { model.signIn("google") } }, Modifier.fillMaxWidth().testTag("hosted-signin-google"), enabled = !model.isBusy) { Text(stringResource(R.string.peppy_hosted_sign_in_google)) }
            }
            "checking_account" -> {
                Heading(R.string.peppy_hosted_account_checking)
                if (model.statusKey == null) OperationProgress(R.string.peppy_hosted_account_checking, "account-checking-progress")
                if (model.statusKey != null) {
                    Action(R.string.peppy_try_again, "account-check-retry", enabled = !model.isBusy) { scope.launch { model.retryAccount() } }
                    OutlinedButton({ clearSecrets(); model.advance("signout") }, Modifier.fillMaxWidth().testTag("account-check-signout")) { Text(stringResource(R.string.peppy_settings_server_sign_out)) }
                }
            }
            "subscribe" -> {
                Heading(R.string.peppy_hosted_subscribe_headline)
                Text(stringResource(R.string.peppy_hosted_subscribe_body))
                Text(model.displayPrice)
                if (model.statusKey == "hosted_subscribe_store_unavailable") {
                    Action(R.string.peppy_try_again, "preview-store-retry", enabled = !model.isBusy) { scope.launch { model.retryStore() } }
                } else {
                    Action(R.string.peppy_hosted_subscribe_cta, "preview-purchase", enabled = !model.isBusy) { model.purchase() }
                }
                Text(stringResource(R.string.peppy_hosted_subscribe_legal), style = MaterialTheme.typography.bodySmall)
            }
            "subscription_verifying" -> {
                Heading(R.string.peppy_hosted_purchase_verifying)
                if (model.localError == null) OperationProgress(R.string.peppy_hosted_purchase_verifying, "subscription-verifying-progress")
                else Action(R.string.peppy_try_again, "subscription-verify-retry", enabled = !model.isBusy) { model.retryVerification() }
            }
            "purchase_pending" -> {
                Heading(R.string.peppy_hosted_purchase_pending)
                Text(stringResource(R.string.peppy_hosted_purchase_pending_body))
                Action(R.string.peppy_try_again, "preview-pending-recheck", enabled = !model.isBusy) { scope.launch { model.retryAccount() } }
            }
            "passphrase" -> {
                Heading(R.string.peppy_passphrase_create_headline)
                Text(stringResource(R.string.peppy_passphrase_create_body))
                Text(stringResource(R.string.peppy_passphrase_irrecoverable_warn))
                if (customMode) {
                    PhraseField(custom, { custom = it; acknowledged = false; confirmation = "" }, R.string.peppy_passphrase_field, "custom-passphrase", revealed)
                    OutlinedButton({ model.generatePassphrase(); custom = ""; customMode = false; revealed = true; acknowledged = false }, Modifier.testTag("toggle-custom-mode")) { Text(stringResource(R.string.peppy_passphrase_use_generated)) }
                } else {
                    if (model.passphrase.isNotEmpty()) {
                        PhraseField(model.passphrase, {}, R.string.peppy_passphrase_field, "generated-passphrase", revealed, readOnly = true)
                        Text(stringResource(R.string.peppy_passphrase_suggestion_note), style = MaterialTheme.typography.bodySmall)
                        OutlinedButton({
                            val clip = ClipData.newPlainText("Peppy", model.passphrase)
                            clip.description.extras = PersistableBundle().apply { putBoolean("android.content.extra.IS_SENSITIVE", true) }
                            context.getSystemService(android.content.ClipboardManager::class.java).setPrimaryClip(clip)
                        }, Modifier.testTag("copy-passphrase")) { Text(stringResource(R.string.peppy_passphrase_copy)) }
                    }
                    OutlinedButton({ model.generatePassphrase(); acknowledged = false; confirmation = ""; revealed = true }, Modifier.testTag("generate-passphrase")) { Text(stringResource(R.string.peppy_passphrase_generate_cta)) }
                    OutlinedButton({ model.clearSecret(); customMode = true; acknowledged = false; confirmation = "" }, Modifier.testTag("toggle-custom-mode")) { Text(stringResource(R.string.peppy_passphrase_custom_cta)) }
                }
                Reveal(revealed) { revealed = !revealed }
                Row(Modifier.fillMaxWidth().toggleable(acknowledged, role = Role.Checkbox) { acknowledged = it }.semantics(mergeDescendants = true) {}.testTag("passphrase-acknowledgement")) {
                    Checkbox(acknowledged, null)
                    Text(stringResource(R.string.peppy_passphrase_ack_label), Modifier.padding(top = 12.dp))
                }
                Button({
                    if (model.createPassphrase(if (customMode) custom else model.passphrase)) {
                        custom = ""; confirmation = ""; acknowledged = false; revealed = false; errorKey = null
                    } else errorKey = "passphrase_weak"
                }, enabled = acknowledged, modifier = Modifier.fillMaxWidth().testTag("create-server")) { Text(stringResource(R.string.peppy_continue)) }
            }
            "confirm" -> {
                Heading(R.string.peppy_passphrase_confirm_headline)
                Text(stringResource(R.string.peppy_passphrase_confirm_body))
                PhraseField(confirmation, { confirmation = it }, R.string.peppy_passphrase_confirm_field, "passphrase-confirmation", revealed)
                Reveal(revealed) { revealed = !revealed }
                Action(R.string.peppy_passphrase_create_submit, "passphrase-confirm-submit") {
                    if (model.confirmCreation(confirmation)) clearSecrets() else errorKey = "passphrase_mismatch"
                }
            }
            "provisioning" -> {
                Heading(R.string.peppy_provisioning_headline)
                if (model.statusKey == "provisioning_error") Action(R.string.peppy_try_again, "provisioning-retry") { model.advance("provision_retry") }
                else OperationProgress(R.string.peppy_provisioning_headline, "provisioning-progress")
            }
            "join", "approval" -> {
                Heading(R.string.peppy_hosted_join_headline)
                Text(stringResource(R.string.peppy_hosted_join_body)); Text("482 731", style = MaterialTheme.typography.headlineMedium)
                Action(R.string.peppy_preview_approval, "preview-show-approval") { model.advance("show_approval") }
                OutlinedButton({ clearSecrets(); model.advance("passphrase_fallback") }, Modifier.testTag("passphrase-fallback-button")) { Text(stringResource(R.string.peppy_hosted_join_fallback)) }
                if (model.screen == "approval") AlertDialog(
                    modifier = Modifier.testTag("device-approval-sheet"),
                    onDismissRequest = { model.advance("cancel") },
                    title = { Text(stringResource(R.string.peppy_device_approval_headline)) },
                    text = { Column { Text(stringResource(R.string.peppy_device_approval_body)); Text("482 731") } },
                    confirmButton = { Button(model::approval, Modifier.testTag("device-allow-button")) { Text(stringResource(R.string.peppy_device_allow)) } },
                    dismissButton = { OutlinedButton({ model.advance("approval_denied") }, Modifier.testTag("device-deny-button")) { Text(stringResource(R.string.peppy_device_deny)) } },
                )
            }
            "unlock" -> {
                Heading(R.string.peppy_hosted_unlock_headline); Text(stringResource(R.string.peppy_hosted_unlock_body))
                PhraseField(custom, { custom = it }, R.string.peppy_passphrase_field, "unlock-passphrase", revealed)
                Reveal(revealed) { revealed = !revealed }
                Action(R.string.peppy_hosted_unlock_cta, "unlock-button") { if (model.unlock(custom)) clearSecrets() else errorKey = "passphrase_mismatch" }
            }
            "syncing" -> {
                Heading(R.string.peppy_hosted_data_preparing)
                if (model.statusKey == "hosted_data_prepare_failed") {
                    Action(R.string.peppy_try_again, "sync-retry", enabled = !model.isBusy) { model.retryPreparation() }
                } else OperationProgress(R.string.peppy_hosted_data_preparing, "syncing-progress")
            }
            "lapsed" -> {
                Heading(R.string.peppy_hosted_lapsed_headline); Text(stringResource(R.string.peppy_hosted_lapsed_body))
                Action(R.string.peppy_hosted_lapsed_resubscribe, "resubscribe-button") { model.advance("resubscribe") }
                OutlinedButton({ clearSecrets(); model.advance("signout") }, Modifier.testTag("lapsed-signout")) { Text(stringResource(R.string.peppy_settings_server_sign_out)) }
            }
            "permissions" -> Permissions { model.advance(it) }
            "settings" -> PreviewSettings(model) { clearSecrets(); model.advance("signout") }
            "delete_account" -> {
                Heading(R.string.peppy_settings_server_delete_title); Text(stringResource(R.string.peppy_settings_server_delete_body))
                Action(R.string.peppy_settings_server_delete, "preview-confirm-delete") { clearSecrets(); model.advance("deletion_confirmed") }
            }
        }
        val reducerStatus = model.statusKey?.takeUnless {
            model.screen in setOf("purchase_pending", "subscription_verifying")
        }
        (errorKey ?: reducerStatus ?: model.localError)?.let { Status(it) }
        if (model.screen !in setOf("welcome", "checking_account")) OutlinedButton({ clearSecrets(); model.advance("back") }, Modifier.testTag("preview-back")) { Text(stringResource(R.string.peppy_back)) }
    }
}

@Composable private fun Heading(label: Int) = Text(stringResource(label), style = MaterialTheme.typography.headlineSmall)
@Composable private fun OperationProgress(label: Int, id: String) {
    val description = stringResource(label)
    CircularProgressIndicator(Modifier.semantics { contentDescription = description }.testTag(id))
}
@Composable private fun Action(label: Int, id: String, enabled: Boolean = true, action: () -> Unit) {
    Button(action, Modifier.fillMaxWidth().testTag(id), enabled = enabled) { Text(stringResource(label)) }
}
@Composable private fun Reveal(revealed: Boolean, action: () -> Unit) {
    OutlinedButton(action, Modifier.testTag("show-hide-passphrase")) { Text(stringResource(if (revealed) R.string.peppy_passphrase_hide else R.string.peppy_passphrase_show)) }
}
@Composable private fun PhraseField(value: String, change: (String) -> Unit, label: Int, id: String, revealed: Boolean, readOnly: Boolean = false) {
    OutlinedTextField(value, change, label = { Text(stringResource(label)) }, readOnly = readOnly,
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password, autoCorrectEnabled = false),
        visualTransformation = if (revealed) VisualTransformation.None else PasswordVisualTransformation(),
        modifier = Modifier.fillMaxWidth().testTag(id))
}
@Composable private fun Permissions(advance: (String) -> Unit) {
    Heading(R.string.peppy_permissions_headline); Text(stringResource(R.string.peppy_permissions_body))
    listOf(R.string.peppy_permissions_contacts, R.string.peppy_permissions_sms, R.string.peppy_permissions_notifications, R.string.peppy_permissions_battery).forEach { label ->
        var checked by remember(label) { mutableStateOf(false) }
        Row(Modifier.fillMaxWidth().toggleable(checked, role = Role.Switch) { checked = it }.semantics(mergeDescendants = true) {}, horizontalArrangement = Arrangement.SpaceBetween) {
            Text(stringResource(label), Modifier.padding(top = 12.dp)); Switch(checked, null)
        }
    }
    Text(stringResource(R.string.peppy_permissions_android_note))
    Action(R.string.peppy_permissions_done, "permissions-done") { advance("permissions_done") }
    OutlinedButton({ advance("permissions_skipped") }) { Text(stringResource(R.string.peppy_permissions_skip)) }
}
@Composable private fun PreviewSettings(model: HostedPreviewModel, signOut: () -> Unit) {
    var manageOpen by remember { mutableStateOf(false) }
    Heading(R.string.peppy_preview_complete)
    ListItem(headlineContent = { Text("peppy.pro") }, supportingContent = { Text(stringResource(R.string.peppy_settings_server_hosted)) })
    ListItem(headlineContent = { Text(model.accountId ?: "preview-account-linked") }, supportingContent = { Text(stringResource(R.string.peppy_settings_server_account)) })
    Text(stringResource(R.string.peppy_settings_server_status)); Status("settings_server_${when (model.entitlement) { "billing_retry" -> "retry"; "none" -> "none"; else -> model.entitlement }}")
    if (model.entitlement in setOf("expired", "revoked")) {
        Text(stringResource(R.string.peppy_hosted_lapsed_body))
        Action(R.string.peppy_hosted_lapsed_resubscribe, "preview-renew") { model.advance("resubscribe") }
    }
    OutlinedButton({ manageOpen = true }, Modifier.testTag("manage-subscription")) { Text(stringResource(R.string.peppy_settings_server_manage)) }
    OutlinedButton(signOut, Modifier.testTag("preview-signout")) { Text(stringResource(R.string.peppy_settings_server_sign_out)) }
    OutlinedButton({ model.advance("delete_account") }, Modifier.testTag("preview-delete-account")) { Text(stringResource(R.string.peppy_settings_server_delete)) }
    if (manageOpen) AlertDialog(onDismissRequest = { manageOpen = false }, title = { Text(stringResource(R.string.peppy_settings_server_manage)) },
        text = { Text(stringResource(R.string.peppy_settings_server_manage_preview)) },
        confirmButton = { Button({ manageOpen = false }) { Text(stringResource(R.string.peppy_continue)) } })
}
@Composable private fun Status(key: String) {
    val resource = when (key) {
        "hosted_purchase_pending" -> R.string.peppy_hosted_purchase_pending
        "hosted_purchase_verifying" -> R.string.peppy_hosted_purchase_verifying
        "hosted_account_check_failed" -> R.string.peppy_hosted_account_check_failed
        "hosted_data_prepare_failed" -> R.string.peppy_hosted_data_prepare_failed
        "hosted_subscribe_store_unavailable" -> R.string.peppy_hosted_subscribe_store_unavailable
        "provisioning_error" -> R.string.peppy_provisioning_error
        "hosted_join_denied" -> R.string.peppy_hosted_join_denied
        "passphrase_weak" -> R.string.peppy_passphrase_weak
        "passphrase_mismatch" -> R.string.peppy_passphrase_mismatch
        "settings_server_grace" -> R.string.peppy_settings_server_grace
        "settings_server_retry" -> R.string.peppy_settings_server_retry
        "settings_server_expired" -> R.string.peppy_settings_server_expired
        "settings_server_revoked" -> R.string.peppy_settings_server_revoked
        "settings_server_active" -> R.string.peppy_settings_server_active
        "settings_server_none" -> R.string.peppy_settings_server_none
        else -> R.string.peppy_preview_error
    }
    Text(stringResource(resource), Modifier.semantics { liveRegion = LiveRegionMode.Polite }.testTag("preview-status"))
}
