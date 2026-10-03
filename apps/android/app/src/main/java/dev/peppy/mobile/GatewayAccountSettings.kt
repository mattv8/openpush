package dev.peppy.mobile

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** Drop-in Account destination; parent calls GatewayAccountSettings(onChanged). */
@Composable
internal fun GatewayAccountSettings(onChanged: () -> Unit) {
    val context = LocalContext.current
    var state by remember { mutableStateOf<GatewayAccountState?>(null) }
    var failure by remember { mutableStateOf<String?>(null) }
    var deleteOpen by remember { mutableStateOf(false) }
    var erase by remember { mutableStateOf("") }
    var revokeTarget by remember { mutableStateOf<GatewayAccountDevice?>(null) }
    val relayPrefs = remember { context.getSharedPreferences("peppy-relay", 0) }
    var relayEnabled by remember { mutableStateOf(relayPrefs.getBoolean("relay-enabled.v1", false)) }
    var relayOrigin by remember { mutableStateOf(relayPrefs.getString("relay-origin.v1", "") ?: "") }
    val scope = rememberCoroutineScope()
    suspend fun refresh() { state = withContext(Dispatchers.IO) { GatewayAccountHost.load(context) }; if (state == null) failure = "Could not load devices. Unlock or check the server connection." }
    LaunchedEffect(Unit) { refresh() }
    Column(Modifier.fillMaxWidth().testTag("account-tab-screen")) {
        val account = state
        if (account == null) Text(failure ?: "Loading account…", Modifier.testTag("devices-loading")) else {
            Section("identity-section", stringResource(R.string.peppy_account)) {
                StatusRow("identity-role", "Role", account.self.role)
                StatusRow("identity-device-id", "This device", account.self.id)
                OutlinedButton(onClick = { failure = null; onChanged() }, modifier = Modifier.testTag("refresh-enrollment-button")) { Text("Refresh from server") }
            }
            Section("devices-section", stringResource(R.string.peppy_devices)) {
                account.devices.forEach { device ->
                    Row(Modifier.fillMaxWidth().testTag("device-row-${device.id}")) {
                        Text("${device.id.take(8)} · ${device.role}${if (device.id == account.self.id) " (this device)" else ""}", Modifier.weight(1f))
                        if (!device.revoked && (device.id == account.self.id || account.self.role == "owner")) OutlinedButton(
                            onClick = { revokeTarget = device },
                            modifier = Modifier.testTag("device-remove-${device.id}"),
                        ) { Text(if (device.id == account.self.id) "Disconnect" else "Remove") }
                    }
                }
            }
            if (account.self.role == "owner") Section("vault-section", "Vault") {
                Button(onClick = { deleteOpen = true }, modifier = Modifier.testTag("vault-delete-button")) { Text("Delete vault") }
            }
        }
        Section("relay-health-section", "Wake relay") {
            OutlinedTextField(relayOrigin, { relayOrigin = it }, label = { Text("Relay HTTPS origin") }, modifier = Modifier.testTag("relay-origin-field"))
            Row(Modifier.fillMaxWidth()) {
                Text("Enable wake relay", Modifier.weight(1f))
                Switch(relayEnabled, { enabled ->
                    relayEnabled = enabled
                    if (enabled) {
                        val origin = CredentialParser.canonicalOrigin(relayOrigin, false)
                        if (origin == null) { relayEnabled = false; failure = "Enter a canonical HTTPS relay origin." } else {
                            RelayWakeRuntime.setEnabled(context, origin, true)
                            if (context.resources.getIdentifier("google_app_id", "string", context.packageName) != 0) {
                                com.google.firebase.messaging.FirebaseMessaging.getInstance().token
                                    .addOnSuccessListener { RelayWakeRuntime.client?.register(it) }
                            }
                        }
                    } else { RelayWakeRuntime.setEnabled(context, null, false) }
                }, modifier = Modifier.testTag("relay-enable-toggle"))
            }
            Text(if (relayEnabled) "Relay registration waits for Firebase configuration." else "No relay configured. Sync runs while the app is open.", Modifier.testTag("relay-not-configured"))
        }
        failure?.let { Text(it, Modifier.testTag("account-error")) }
    }
    if (deleteOpen) AlertDialog(
        onDismissRequest = { deleteOpen = false }, title = { Text("Type ERASE to confirm") },
        text = { OutlinedTextField(erase, { erase = it }, label = { Text("Type ERASE") }, modifier = Modifier.testTag("vault-delete-confirm-field")) },
        confirmButton = { Button(onClick = { scope.launch { val ok = withContext(Dispatchers.IO) { GatewayAccountHost.deleteVault(context) }; if (!ok) failure = "Could not reach the server. The vault was not deleted." else onChanged() }; deleteOpen = false }, enabled = erase == "ERASE", modifier = Modifier.testTag("vault-delete-final-button")) { Text("Delete vault") } },
        dismissButton = { OutlinedButton(onClick = { deleteOpen = false }) { Text("Keep vault") } },
    )
    revokeTarget?.let { target ->
        val own = state?.self?.id == target.id
        AlertDialog(
            onDismissRequest = { revokeTarget = null },
            title = { Text(if (own) "Disconnect this phone?" else "Remove device?") },
            text = { Text(if (own) "This stops Peppy locally. If offline, the server credential remains active until a later revoke succeeds." else "This device will lose access to the Peppy server.") },
            confirmButton = { Button(onClick = {
                scope.launch {
                    val ok = withContext(Dispatchers.IO) { GatewayAccountHost.revoke(context, target.id) }
                    if (!ok) failure = if (own) "Could not revoke. Peppy remains locally available and the server token is still active." else "Could not remove device."
                    else { revokeTarget = null; refresh(); onChanged() }
                }
            }, modifier = Modifier.testTag("device-revoke-confirm-${target.id}")) { Text(if (own) "Disconnect" else "Remove") } },
            dismissButton = { OutlinedButton(onClick = { revokeTarget = null }) { Text("Cancel") } },
        )
    }
}
