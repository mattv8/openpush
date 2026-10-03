package dev.peppy.mobile

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.ContextWrapper
import android.content.Intent
import android.net.Uri
import android.provider.Settings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.foundation.text.KeyboardOptions
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

internal sealed interface OwnNumberUiState {
    data class Detected(val number: String, val saved: String?) : OwnNumberUiState
    data class NoAccess(val saved: String?) : OwnNumberUiState
    data class Manual(val saved: String?) : OwnNumberUiState
    data object Loading : OwnNumberUiState
    data object Unavailable : OwnNumberUiState
}

internal fun ownNumberUiState(
    databaseOpen: Boolean,
    route: SimRoute?,
    detection: OwnNumberDetection?,
    saved: String?,
): OwnNumberUiState = when {
    !databaseOpen || route == null -> OwnNumberUiState.Unavailable
    detection == null -> OwnNumberUiState.Loading
    detection is OwnNumberDetection.Detected -> OwnNumberUiState.Detected(detection.number, saved)
    detection is OwnNumberDetection.NoAccess -> OwnNumberUiState.NoAccess(saved)
    else -> OwnNumberUiState.Manual(saved)
}

private fun Context.findActivity(): Activity? {
    var current: Context = this
    while (current is ContextWrapper) {
        if (current is Activity) return current
        current = current.baseContext
    }
    return current as? Activity
}

@Composable
internal fun OwnNumberSettings(context: Context, databaseOpen: Boolean, route: SimRoute?) {
    var detection by remember(route, databaseOpen) { mutableStateOf<OwnNumberDetection?>(null) }
    var saved by remember(route, databaseOpen) { mutableStateOf<String?>(null) }
    var draft by remember(route) { mutableStateOf("") }
    var editing by remember(route) { mutableStateOf(false) }
    var message by remember(route) { mutableStateOf<String?>(null) }
    var permissionDenied by remember { mutableStateOf(false) }
    var resumeRefresh by remember { mutableIntStateOf(0) }
    val scope = rememberCoroutineScope()
    val lifecycleOwner = LocalLifecycleOwner.current

    fun reload() {
        if (!databaseOpen || route == null) {
            detection = null
            saved = null
            return
        }
        scope.launch {
            val result = withContext(Dispatchers.IO) {
                val detected = try {
                    OwnNumberDetector.detect(context, route)
                } catch (_: Exception) {
                    OwnNumberDetection.Unavailable
                }
                try {
                    val client = NativeGateway.open(context) ?: return@withContext detected to null
                    val existing = client.mmsOwnAddress(route.routeId)
                    val savedNumber = OwnNumberDetector.seedDetectedNumber(existing, detected) { number ->
                        client.setMmsOwnAddress(route.routeId, number)
                    }
                    detected to savedNumber
                } catch (_: Exception) {
                    detected to null
                }
            }
            detection = result.first
            saved = result.second
        }
    }

    fun saveManual() {
        val address = draft.trim()
        if (route != null) scope.launch {
            val error = withContext(Dispatchers.IO) {
                try {
                    val client = NativeGateway.open(context) ?: throw uniffi.peppy_mobile_bindings.MobileBindingsException.Closed()
                    client.setMmsOwnAddress(route.routeId, address)
                    null
                } catch (error: Exception) {
                    error
                }
            }
            if (error == null) {
                saved = address
                editing = false
                draft = ""
                message = "Own number saved for this SIM."
            } else message = ownAddressError(error)
        }
    }

    val permissionRequest = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        permissionDenied = !granted
        reload()
    }

    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, event -> if (event == Lifecycle.Event.ON_RESUME) resumeRefresh++ }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose { lifecycleOwner.lifecycle.removeObserver(observer) }
    }
    LaunchedEffect(route, databaseOpen, resumeRefresh) { reload() }

    val state = ownNumberUiState(databaseOpen, route, detection, saved)
    val activity = context.findActivity()
    val permissionPermanentlyDenied = permissionDenied &&
        activity?.shouldShowRequestPermissionRationale(Manifest.permission.READ_PHONE_NUMBERS) == false

    Column(
        modifier = Modifier.fillMaxWidth().testTag("mms-own-number-block"),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        when (state) {
            OwnNumberUiState.Loading -> {
                StatusRow("mms-own-number-row", "This SIM's number", "Checking…")
            }
            is OwnNumberUiState.Detected -> {
                StatusRow("mms-own-number-row", "This SIM's number", state.saved ?: state.number)
                StatusRow("mms-own-number-source", "Source", if (state.saved == null || state.saved == state.number) "From carrier" else "Entered manually")
                ManualEntryControls(
                    saved = state.saved ?: state.number,
                    editing = editing,
                    draft = draft,
                    onEdit = {
                        draft = state.saved ?: state.number
                        editing = true
                        message = null
                    },
                    onDraftChanged = { draft = it },
                    onCancel = { editing = false; draft = "" },
                    onSave = { saveManual() },
                )
            }
            is OwnNumberUiState.NoAccess -> {
                StatusRow("mms-own-number-row", "This SIM's number", state.saved ?: "Not detected")
                StatusRow("mms-own-number-source", "Source", "Permission needed")
                Text("Needed only so replies to group MMS threads exclude this phone.", style = MaterialTheme.typography.bodySmall)
                OutlinedButton(
                    onClick = {
                        if (permissionPermanentlyDenied && activity != null) {
                            activity.startActivity(Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS).setData(Uri.parse("package:${context.packageName}")))
                        } else {
                            permissionRequest.launch(Manifest.permission.READ_PHONE_NUMBERS)
                        }
                    },
                    modifier = Modifier.testTag("mms-own-number-permission-button"),
                ) { Text(if (permissionPermanentlyDenied) "Open app settings" else "Allow phone number access") }
                ManualEntryControls(
                    saved = state.saved,
                    editing = editing,
                    draft = draft,
                    onEdit = {
                        draft = state.saved.orEmpty()
                        editing = true
                        message = null
                    },
                    onDraftChanged = { draft = it },
                    onCancel = { editing = false; draft = "" },
                    onSave = { saveManual() },
                    outlined = false,
                )
            }
            is OwnNumberUiState.Manual -> {
                StatusRow("mms-own-number-row", "This SIM's number", state.saved ?: "Not set")
                if (state.saved != null) StatusRow("mms-own-number-source", "Source", "Entered manually")
                if (state.saved == null) Text("Only needed so replies to group MMS threads exclude this phone.", style = MaterialTheme.typography.bodySmall)
                ManualEntryControls(
                    saved = state.saved,
                    editing = editing,
                    draft = draft,
                    onEdit = {
                        draft = state.saved.orEmpty()
                        editing = true
                        message = null
                    },
                    onDraftChanged = { draft = it },
                    onCancel = { editing = false; draft = "" },
                    onSave = { saveManual() },
                )
            }
            OwnNumberUiState.Unavailable -> StatusRow("mms-own-number-row", "This SIM's number", "Unavailable")
        }
        message?.let { Text(it, Modifier.testTag("mms-own-number-message"), style = MaterialTheme.typography.bodySmall) }
    }
}

@Composable
private fun ManualEntryControls(
    saved: String?,
    editing: Boolean,
    draft: String,
    onEdit: () -> Unit,
    onDraftChanged: (String) -> Unit,
    onCancel: () -> Unit,
    onSave: () -> Unit,
    outlined: Boolean = true,
) {
    if (!editing) {
        val modifier = Modifier.testTag(if (saved == null) "mms-own-number-enter-button" else "mms-own-number-change-button")
        if (outlined) {
            OutlinedButton(onClick = onEdit, modifier = modifier) { Text(if (saved == null) "Enter number" else "Change") }
        } else {
            TextButton(onClick = onEdit, modifier = modifier) { Text(if (saved == null) "Enter number" else "Change") }
        }
    } else {
        OutlinedTextField(
            value = draft,
            onValueChange = onDraftChanged,
            label = { Text("Your number for this SIM") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Phone),
            modifier = Modifier.fillMaxWidth().testTag("mms-own-number-field"),
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = onSave, enabled = draft.isNotBlank(), modifier = Modifier.testTag("mms-own-number-save")) { Text("Save") }
            TextButton(onClick = onCancel, modifier = Modifier.testTag("mms-own-number-cancel")) { Text("Cancel") }
        }
    }
}
