package dev.peppy.mobile

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.graphics.Color
import android.net.Uri
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.Image
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Notifications
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.Sms
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Scaffold
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.ExperimentalComposeUiApi
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import dev.peppy.mobile.ui.theme.PeppyTheme
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.ByteArrayOutputStream
import java.io.IOException

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val systemBars = SystemBarStyle.auto(Color.TRANSPARENT, Color.TRANSPARENT)
        enableEdgeToEdge(statusBarStyle = systemBars, navigationBarStyle = systemBars)
        setContent { PeppyTheme { Surface { CompanionScreen() } } }
    }
}

private val SMS_PERMISSIONS = arrayOf(Manifest.permission.RECEIVE_SMS, Manifest.permission.SEND_SMS)
private val MMS_PERMISSIONS = arrayOf(Manifest.permission.READ_SMS, Manifest.permission.RECEIVE_MMS)

private fun granted(context: Context, permission: String) =
    context.checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED

/** Reads at most [CredentialParser.MAX_BYTES] + 1 bytes so oversized files are rejected unread. */
private fun readBounded(context: Context, uri: Uri): ByteArray? = try {
    context.contentResolver.openInputStream(uri)?.use { input ->
        val out = ByteArrayOutputStream()
        val buffer = ByteArray(4096)
        while (out.size() <= CredentialParser.MAX_BYTES) {
            val read = input.read(buffer)
            if (read < 0) break
            out.write(buffer, 0, read)
        }
        out.toByteArray()
    }
} catch (_: IOException) {
    null
} catch (_: SecurityException) {
    null
}

internal fun importMessage(result: ImportResult): String = when (result) {
    ImportResult.IMPORTED -> "Credential verified for this gateway device. Unlock with the vault passphrase next."
    ImportResult.UPDATED -> "Credential refreshed for the same device. Local data and its protected key were kept."
    ImportResult.INVALID_CREDENTIAL -> "This file is not a valid Peppy v1 device credential (16 KiB maximum)."
    ImportResult.SERVER_REJECTED -> "The server did not accept this credential for its vault and device."
    ImportResult.NOT_A_GATEWAY -> "This credential belongs to a viewing device. Pair this phone as an SMS gateway instead."
    ImportResult.NETWORK_ERROR -> "Could not reach the Peppy server. Check the connection and try again."
    ImportResult.IDENTITY_MISMATCH -> "This phone is already bound to a different server, vault, or device. Import refused to protect existing data."
    ImportResult.EXISTING_DATA_WITHOUT_KEY -> "Local Peppy data exists without its protected key. Import refused so that data is not overwritten."
    ImportResult.STORAGE_ERROR -> "Secure storage on this phone is unavailable. Nothing was saved."
}

internal fun unlockMessage(result: UnlockResult): String = when (result) {
    UnlockResult.UNLOCKED -> "Vault keys unlocked. Background sync is enabled."
    UnlockResult.NOT_ENROLLED -> "Import a device credential first."
    UnlockResult.DATABASE_UNAVAILABLE -> "The local encrypted database could not be opened with this phone's protected key."
    UnlockResult.WRONG_PASSPHRASE -> "That passphrase does not match this vault."
    UnlockResult.SUPERSEDED -> "The credential changed while unlocking. Enter the passphrase again."
    UnlockResult.FAILED -> "Unlock failed. Try again."
}

@Composable
internal fun syncMessage(status: GatewayStatus): String = when {
    status.syncPhase == SyncPhase.RECOVERY_REQUIRED -> when (status.recoveryReason) {
        RecoveryReason.PRODUCER_CONFLICT -> "Stopped: the server reports this device's updates conflict with an earlier install. Pair this phone again as a new gateway device."
        RecoveryReason.DATABASE_MISSING -> "Stopped: this phone's encrypted message database is missing, so it cannot safely continue. Nothing will be sent. Pair this phone again as a new gateway device."
        else -> "Stopped: this credential was already used by an earlier install. Pair this phone again as a new gateway device."
    }
    status.outboxRejection != null -> "Stopped: the server permanently rejected a queued update (${status.outboxRejection}). Sending and uploads are paused; pair this phone again."
    !status.sharedKeysReady -> stringResource(R.string.peppy_not_unlocked)
    status.syncPhase == SyncPhase.LIVE -> "Live"
    else -> "Importing vault history (no texts are sent until this finishes)"
}

@OptIn(ExperimentalComposeUiApi::class, ExperimentalMaterial3Api::class)
@Composable
private fun CompanionScreen() {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var status by remember { mutableStateOf<GatewayStatus?>(null) }
    var refresh by remember { mutableIntStateOf(0) }
    var importBusy by remember { mutableStateOf(false) }
    var importResult by remember { mutableStateOf<ImportResult?>(null) }
    var unlockBusy by remember { mutableStateOf(false) }
    var unlockResult by remember { mutableStateOf<UnlockResult?>(null) }
    var passphrase by remember { mutableStateOf("") }
    var smsCaptureEnabled by remember { mutableStateOf(GatewayPolicyHost(context).smsCaptureEnabled) }
    var destination by remember { mutableStateOf("sms") }
    var settingsOpen by remember { mutableStateOf(false) }
    var pairingOpen by remember { mutableStateOf(false) }
    var hostedPreviewOpen by remember { mutableStateOf(BuildConfig.DEBUG) }

    LaunchedEffect(refresh) {
        val current = withContext(Dispatchers.IO) { NativeGateway.status(context) }
        if (current.sharedKeysReady) GatewayWork.ensurePeriodic(context)
        if (MmsPreferences(context).enabled) MmsCaptureWork.ensurePeriodic(context)
        status = current
    }

    val filePicker = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri == null) return@rememberLauncherForActivityResult
        importBusy = true
        scope.launch {
            importResult = withContext(Dispatchers.IO) {
                readBounded(context, uri)?.let { NativeGateway.importCredential(context, it) } ?: ImportResult.INVALID_CREDENTIAL
            }
            importBusy = false
            refresh++
        }
    }
    val permissionRequest = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
        refresh++
        if (status?.sharedKeysReady == true) GatewayWork.enqueue(context)
    }
    val mmsPermissionRequest = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
        if (MmsPreferences(context).enabled && mmsReceiveGranted(context)) MmsCaptureWork.enqueue(context)
        refresh++
    }
    val contactsPermissionRequest = rememberLauncherForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
        ContactSyncHost.ensureObserver(context)
        if (ContactSyncPreferences(context).enabled) GatewayScheduler.schedule(context)
        refresh++
    }

    if (pairingOpen) {
        PairingScreen(
            onDismiss = { pairingOpen = false },
            onFinished = {
                pairingOpen = false
                refresh++
            },
        )
        return
    }

    // The fake hosted route is compiled in but inaccessible in release builds and for
    // a real enrollment. It never shares the gateway's credentials or state.
    if (status == null) {
        CircularProgressIndicator(Modifier.padding(24.dp))
        return
    }
    if (BuildConfig.DEBUG && hostedPreviewOpen && status?.enrolled == false) {
        HostedOnboardingScreen(onSelfHosted = { hostedPreviewOpen = false })
        return
    }

    BackHandler(enabled = settingsOpen) { settingsOpen = false }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(if (settingsOpen) R.string.peppy_settings else R.string.peppy_app_name)) },
                actions = {
                    if (status?.enrolled == true) {
                        IconButton(onClick = { settingsOpen = !settingsOpen }, modifier = Modifier.testTag("settings-button")) {
                            Icon(Icons.Default.Settings, contentDescription = if (settingsOpen) "Close settings" else "Open settings")
                        }
                    }
                },
            )
        },
        bottomBar = {
            if (status?.enrolled == true && status?.sharedKeysReady == true && !settingsOpen) NavigationBar {
                listOf("mirroring" to R.string.peppy_mirroring, "sms" to R.string.peppy_sms, "account" to R.string.peppy_account).forEach { (route, labelRes) ->
                    val label = stringResource(labelRes)
                    val icon = when (route) { "mirroring" -> Icons.Default.Notifications; "sms" -> Icons.Default.Sms; else -> Icons.Default.Settings }
                    NavigationBarItem(selected = destination == route, onClick = { destination = route }, icon = { Icon(icon, contentDescription = label) }, label = { Text(label) }, modifier = Modifier.testTag("tab-$route"))
                }
            }
        },
    ) { innerPadding -> Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(innerPadding).padding(24.dp)
            .semantics { testTagsAsResourceId = true }.testTag("companion-screen"),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        if (status?.enrolled != true && !settingsOpen) Column(
            Modifier.fillMaxWidth().testTag("welcome-screen"),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Column(Modifier.fillMaxWidth().testTag("self-hosted-screen"), verticalArrangement = Arrangement.spacedBy(16.dp)) {
            Image(
                painter = painterResource(R.drawable.peppy_logo),
                contentDescription = null,
                modifier = Modifier.size(88.dp).testTag("pairing-hero"),
            )
            if (BuildConfig.DEBUG) {
                OutlinedButton(onClick = { hostedPreviewOpen = true }, modifier = Modifier.testTag("self-hosted-back")) {
                    Text(stringResource(R.string.peppy_back))
                }
            }
            Text(stringResource(R.string.peppy_self_hosted_headline), style = MaterialTheme.typography.headlineSmall)
            Text(stringResource(R.string.peppy_self_hosted_body), style = MaterialTheme.typography.bodyMedium)
            Button(onClick = { pairingOpen = true }, modifier = Modifier.fillMaxWidth().testTag("pair-qr-button")) { Text("Scan QR code") }
            BusyButton("import-credential-button", "Import credential file", importBusy, enabled = !importBusy && !unlockBusy, modifier = Modifier.fillMaxWidth()) {
                filePicker.launch(arrayOf("application/json", "text/plain", "application/octet-stream"))
            }
            importResult?.let { Text(importMessage(it), Modifier.testTag("enroll-error"), style = MaterialTheme.typography.bodySmall) }
            OutlinedButton({ context.startActivity(android.content.Intent(android.content.Intent.ACTION_VIEW, Uri.parse("https://github.com/mattv8/peppy#readme"))) }, Modifier.testTag("self-hosted-docs-link")) { Text(stringResource(R.string.peppy_self_hosted_docs_link)) }
            }
        }

        if (status?.enrolled == true && !status!!.sharedKeysReady && !settingsOpen) Section("lock-screen", stringResource(R.string.peppy_locked)) {
            Text(
                "Enter the existing encryption passphrase you already use on your other devices. This phone does not create a new passphrase.",
                style = MaterialTheme.typography.bodyMedium,
            )
            OutlinedTextField(
                value = passphrase,
                onValueChange = { passphrase = it },
                label = { Text(stringResource(R.string.peppy_passphrase_field)) },
                singleLine = true,
                enabled = !unlockBusy && status?.enrolled == true,
                visualTransformation = PasswordVisualTransformation(),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
                modifier = Modifier.fillMaxWidth().testTag("passphrase-field"),
            )
            BusyButton(
                "unlock-button", "Unlock", unlockBusy,
                enabled = !unlockBusy && !importBusy && status?.enrolled == true && passphrase.isNotEmpty(),
            ) {
                val attempt = passphrase
                passphrase = ""
                unlockBusy = true
                scope.launch {
                    val result = withContext(Dispatchers.IO) { NativeGateway.unlock(context, attempt) }
                    if (result == UnlockResult.UNLOCKED) { NotificationMirrorService.reconcile(); GatewayWork.enqueue(context) }
                    unlockResult = result
                    unlockBusy = false
                    refresh++
                }
            }
            unlockResult?.let { Text(unlockMessage(it), Modifier.testTag("unlock-error"), style = MaterialTheme.typography.bodySmall) }
        }

        if (!settingsOpen && status?.enrolled == true && destination == "sms") Section("sms-tab-screen", stringResource(R.string.peppy_sms)) {
        Section("status-section", "Gateway status") {
            val current = status
            if (current == null) {
                CircularProgressIndicator(Modifier.size(20.dp).testTag("status-loading"), strokeWidth = 2.dp)
            } else {
                StatusRow("status-credential", "Device credential", if (current.enrolled) "Imported (${current.origin})" else "Not imported")
                StatusRow("status-database", "Encrypted local database", if (current.databaseOpen) "Open" else "Not open")
                StatusRow("status-vault-keys", "Vault keys", if (current.sharedKeysReady) "Unlocked" else "Locked — unlock to sync")
                if (current.enrolled) {
                    Text(syncMessage(current), Modifier.testTag("status-sync"), style = MaterialTheme.typography.bodyMedium)
                }
                StatusRow("status-receive", "Receive SMS", if (granted(context, Manifest.permission.RECEIVE_SMS)) "Allowed" else "Not allowed")
                StatusRow("status-send", "Send SMS", if (granted(context, Manifest.permission.SEND_SMS)) "Allowed" else "Not allowed")
                StatusRow("status-sim", "SIM route", if (SimRoutes.current().isEmpty()) "No default SMS SIM" else "Default SMS SIM")
            }
        }

        Section("permissions-section", "SMS permissions") {
            Text(
                "Receive SMS lets this phone save incoming texts, encrypted, to your Peppy server. Send SMS lets it send texts you queue from " +
                    "your other devices, only after this phone's vault confirms each one. The SMS inbox, contacts, and calls are not read, " +
                    "and Peppy never becomes the default SMS app.",
                style = MaterialTheme.typography.bodyMedium,
            )
            OutlinedButton(onClick = { permissionRequest.launch(SMS_PERMISSIONS) }, modifier = Modifier.testTag("permissions-button")) {
                Text("Allow SMS permissions")
            }
        }

        Section("battery-section", "Battery") {
            val unrestricted = context.getSystemService(PowerManager::class.java)
                ?.isIgnoringBatteryOptimizations(context.packageName) == true
            StatusRow("battery-optimization-status", "Battery optimization", if (unrestricted) "Unrestricted" else "May delay background sync")
            OutlinedButton(
                onClick = { context.startActivity(android.content.Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)) },
                modifier = Modifier.testTag("battery-optimization-settings-button"),
            ) { Text("Open battery settings") }
        }

        Section("gateway-settings-section", "Gateway settings") {
            Row(Modifier.fillMaxWidth().testTag("settings-sms-capture-toggle"), horizontalArrangement = Arrangement.SpaceBetween) {
                Column(Modifier.weight(1f)) {
                    Text("Enable SMS capture")
                    Text("Stops new incoming carrier captures without changing pending or unknown send permits.", style = MaterialTheme.typography.bodySmall)
                }
                Switch(smsCaptureEnabled, { value ->
                    GatewayPolicyHost(context).smsCaptureEnabled = value
                    smsCaptureEnabled = value
                })
            }
        }

        MmsSettings(
            context = context,
            refresh = refresh,
            databaseOpen = status?.databaseOpen == true,
            requestPermissions = { mmsPermissionRequest.launch(MMS_PERMISSIONS) },
            onChanged = { refresh++ },
        )

        Section("limitations-section", "Messaging limits") {
            Bullet("limitation-mms", "MMS messages sync after the phone finishes downloading them and uploading encrypted copies.")
            Bullet("limitation-rcs", "RCS requires a verified carrier or OEM integration and is not available in this build.")
            Bullet(
                "limitation-background",
                "Android can delay background sync in Doze or battery saver. Force-stopping Peppy pauses capture until you open it again. " +
                    "Some one-time-code texts may not reach companion apps.",
            )
            Bullet("limitation-carrier", stringResource(R.string.peppy_carrier_not_encrypted))
        }
        }
        if (!settingsOpen && status?.enrolled == true && destination == "mirroring") NotificationMirroringSettings(refresh = refresh, onChanged = { refresh++ })
        if (!settingsOpen && status?.enrolled == true && destination == "account") GatewayAccountSettings(onChanged = { refresh++ })
        if (settingsOpen) {
            Section("settings-screen", "Contacts") {
                ContactSyncSettings(context = context, refresh = refresh, requestPermissions = { contactsPermissionRequest.launch(CONTACT_PERMISSIONS) }, onChanged = { refresh++ })
            }
            Section("about-section", "About") {
                Text(stringResource(R.string.peppy_encrypted))
                StatusRow("settings-server", "Server", status?.origin ?: "Not enrolled")
            }
        }
    } }
}



@Composable
internal fun Section(tag: String, title: String, content: @Composable () -> Unit) {
    Column(Modifier.fillMaxWidth().testTag(tag), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        HorizontalDivider()
        Text(title, style = MaterialTheme.typography.titleMedium)
        content()
    }
}

@Composable
private fun BusyButton(tag: String, label: String, busy: Boolean, enabled: Boolean, modifier: Modifier = Modifier, onClick: () -> Unit) {
    Button(onClick = onClick, enabled = enabled, modifier = modifier.testTag(tag)) {
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            if (busy) CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
            Text(label)
        }
    }
}

@Composable
internal fun StatusRow(tag: String, label: String, value: String) {
    Row(Modifier.fillMaxWidth().testTag(tag), horizontalArrangement = Arrangement.SpaceBetween) {
        Text(label, style = MaterialTheme.typography.bodyMedium)
        Text(value, style = MaterialTheme.typography.bodyMedium)
    }
}

@Composable
private fun Bullet(tag: String, text: String) {
    Text("• $text", Modifier.testTag(tag), style = MaterialTheme.typography.bodySmall)
}
