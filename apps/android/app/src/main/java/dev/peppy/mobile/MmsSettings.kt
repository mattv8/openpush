package dev.peppy.mobile

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import org.json.JSONArray
import org.json.JSONObject
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Button
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.MaterialTheme
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import uniffi.peppy_mobile_bindings.MobileBindingsException
import uniffi.peppy_mobile_bindings.NativeMmsAcquisitionState

internal data class MmsHealth(
    val pending: Int = 0,
    val blocked: Int = 0,
    val unavailable: Int = 0,
    val reasons: List<String> = emptyList(),
    val transferReasons: List<String> = emptyList(),
    val databaseUnavailable: Boolean = false,
)

/** Bounded, content-free transfer failures shown only on this phone. */
internal class MmsTransferHealth(context: Context) {
    private val prefs = context.applicationContext.getSharedPreferences("peppy-mms-transfer-health", Context.MODE_PRIVATE)

    fun record(failures: List<MmsTransferFailure>) {
        if (failures.isEmpty()) return
        val entries = JSONArray()
        failures.take(MAX_ISSUES).forEach { failure ->
            entries.put(JSONObject().put("attachment", failure.attachmentId).put("operation", failure.operation).put("reason", failure.reason))
        }
        prefs.edit().putString(ISSUES, entries.toString()).apply()
    }

    fun reasons(): List<String> = try {
        val entries = JSONArray(prefs.getString(ISSUES, "[]"))
        (0 until entries.length()).mapNotNull { index ->
            val entry = entries.optJSONObject(index) ?: return@mapNotNull null
            val operation = entry.optString("operation")
            val reason = entry.optString("reason")
            if (operation in OPERATIONS && reason in REASONS) "$operation: $reason" else null
        }
    } catch (_: Exception) {
        emptyList()
    }

    private companion object {
        const val ISSUES = "issues"
        const val MAX_ISSUES = 8
        val OPERATIONS = setOf("upload", "download")
        val REASONS = setOf("transient", "auth", "quota", "permanent", "invalid_media")
    }
}

internal fun mmsReceiveGranted(context: Context) =
    context.checkSelfPermission(Manifest.permission.READ_SMS) == PackageManager.PERMISSION_GRANTED &&
        context.checkSelfPermission(Manifest.permission.RECEIVE_MMS) == PackageManager.PERMISSION_GRANTED

private fun loadMmsHealth(context: Context, databaseOpen: Boolean): MmsHealth {
    if (!databaseOpen) return MmsHealth(databaseUnavailable = true, transferReasons = MmsTransferHealth(context).reasons())
    val client = NativeGateway.open(context) ?: return MmsHealth(databaseUnavailable = true, transferReasons = MmsTransferHealth(context).reasons())
    val acquisitions = client.mmsAcquisitions(1000uL)
    val incomplete = acquisitions.filter { it.state != NativeMmsAcquisitionState.COMPLETE }
    return MmsHealth(
        pending = incomplete.count { it.state == NativeMmsAcquisitionState.PENDING },
        blocked = incomplete.count { it.state == NativeMmsAcquisitionState.BLOCKED },
        unavailable = incomplete.count { it.state == NativeMmsAcquisitionState.UNAVAILABLE },
        reasons = incomplete.mapNotNull { it.reason?.take(120) }.distinct().take(8),
        transferReasons = MmsTransferHealth(context).reasons(),
    )
}

internal fun ownAddressError(error: Throwable?): String = when (error) {
    is MobileBindingsException.InvalidRequest -> "This number could not be saved for the selected SIM."
    is MobileBindingsException.Closed -> "The encrypted database is unavailable. Import or unlock this gateway first."
    is MobileBindingsException.InvalidDatabaseKey,
    is MobileBindingsException.WrongDatabaseKey,
    is MobileBindingsException.Database,
    is MobileBindingsException.UnsupportedSchema,
    is MobileBindingsException.Crypto -> "The encrypted database is unavailable. Try again after reopening the gateway."
    else -> "Could not save the number securely. Try again."
}

/** Local-only companion controls. Own addresses are committed directly to SQLCipher, never prefs. */
@Composable
internal fun MmsSettings(
    context: Context,
    refresh: Int,
    databaseOpen: Boolean,
    requestPermissions: () -> Unit,
    onChanged: () -> Unit,
) {
    val preferences = remember { MmsPreferences(context) }
    var enabled by remember(refresh) { mutableStateOf(preferences.enabled) }
    var historyRequested by remember(refresh) { mutableStateOf(preferences.importHistory) }
    var number by remember { mutableStateOf("") }
    var message by remember { mutableStateOf<String?>(null) }
    var health by remember { mutableStateOf(MmsHealth()) }
    val scope = rememberCoroutineScope()
    val route = SimRoutes.current().singleOrNull()
    val lifecycleOwner = LocalLifecycleOwner.current
    var permissionRefresh by remember { mutableIntStateOf(0) }
    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, event -> if (event == Lifecycle.Event.ON_RESUME) permissionRefresh++ }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose { lifecycleOwner.lifecycle.removeObserver(observer) }
    }
    val receiveGranted = permissionRefresh.let { mmsReceiveGranted(context) }

    LaunchedEffect(refresh, enabled, databaseOpen) {
        health = withContext(Dispatchers.IO) { loadMmsHealth(context, databaseOpen) }
    }

    Section("mms-settings-section", "MMS") {
        Text(
            "Sync MMS after your phone's messaging app downloads it. Messages appear on other devices after encrypted uploads complete.",
            style = MaterialTheme.typography.bodyMedium,
        )
        Row(Modifier.fillMaxWidth().testTag("mms-enable-row"), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(
                onClick = {
                    preferences.enabled = !enabled
                    enabled = preferences.enabled
                    if (enabled) MmsCaptureWork.ensurePeriodic(context)
                    onChanged()
                },
                modifier = Modifier.testTag("mms-enable-button"),
            ) { Text(if (enabled) "Disable MMS" else "Enable MMS") }
        }
        StatusRow("mms-permission-status", "MMS read permission", if (receiveGranted) "Allowed" else "Not allowed")
        if (!receiveGranted) {
            OutlinedButton(onClick = requestPermissions, modifier = Modifier.testTag("mms-permission-button")) { Text("Allow MMS permissions") }
        }
        StatusRow("mms-sim-status", "Current SIM", route?.label ?: "No default SMS SIM")
        OutlinedTextField(
            value = number,
            onValueChange = { number = it },
            label = { Text("Your number for this SIM") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Phone),
            // Own-address metadata is encrypted local configuration, intentionally available before
            // capture permissions so a user can prepare the current route without provider access.
            enabled = databaseOpen && route != null,
            modifier = Modifier.fillMaxWidth().testTag("mms-own-number-field"),
        )
        Button(
            onClick = {
                val address = number.trim()
                if (address.isBlank() || route == null) {
                    message = "Enter a number and choose a default SMS SIM."
                } else {
                    scope.launch {
                        val result: Throwable? = withContext(Dispatchers.IO) {
                            try {
                                val client = NativeGateway.open(context)
                                if (client == null) MobileBindingsException.Closed() else {
                                    client.setMmsOwnAddress(route.routeId, address)
                                    null
                                }
                            } catch (error: MobileBindingsException) {
                                error
                            } catch (error: Exception) {
                                error
                            }
                        }
                        if (result == null) {
                            message = "Own number saved securely for this SIM."
                            number = ""
                        } else {
                            message = ownAddressError(result)
                        }
                    }
                }
            },
            enabled = databaseOpen && route != null && number.isNotBlank(),
            modifier = Modifier.testTag("mms-own-number-confirm"),
        ) { Text("Confirm own number") }
        OutlinedButton(
            onClick = {
                preferences.importHistory = true
                historyRequested = true
                MmsCaptureWork.enqueue(context)
                onChanged()
            },
            enabled = enabled && receiveGranted && !historyRequested,
            modifier = Modifier.testTag("mms-import-history"),
        ) { Text(if (historyRequested) "MMS history import queued" else "Import MMS history") }
        StatusRow("mms-history-status", "MMS history import", if (historyRequested) "Queued" else "Not requested")
        OutlinedButton(
            onClick = { MmsCaptureWork.refresh(context); onChanged() },
            enabled = enabled && receiveGranted,
            modifier = Modifier.testTag("mms-refresh-pending"),
        ) { Text("Refresh pending MMS") }
        message?.let { Text(it, Modifier.testTag("mms-own-number-message"), style = MaterialTheme.typography.bodySmall) }
        Column(Modifier.fillMaxWidth().testTag("mms-health-list"), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            when {
                health.databaseUnavailable -> Text("Encrypted database unavailable. Acquisition health cannot be read.")
                health.pending == 0 && health.blocked == 0 && health.unavailable == 0 -> Text("No pending acquisitions.")
                else -> Text("Phone-only acquisition health: ${health.pending} pending, ${health.blocked} blocked, ${health.unavailable} unavailable.")
            }
            health.reasons.forEach { Text(it, Modifier.testTag("mms-health-reason"), style = MaterialTheme.typography.bodySmall) }
            health.transferReasons.forEach { Text("Transfer $it", Modifier.testTag("mms-transfer-health-reason"), style = MaterialTheme.typography.bodySmall) }
        }
    }
}
