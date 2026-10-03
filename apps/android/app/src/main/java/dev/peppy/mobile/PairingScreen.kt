package dev.peppy.mobile

import android.Manifest
import android.content.pm.PackageManager
import android.provider.Settings
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext

private sealed interface PairingState {
    data object Scanning : PairingState
    data class Claiming(val payload: PairingPayload) : PairingState
    data class Decoded(val payload: PairingPayload, val claim: PairingClaim) : PairingState
    data class Approving(val payload: PairingPayload) : PairingState
    data class Error(val message: String) : PairingState
}

@Composable
internal fun PairingScreen(onDismiss: () -> Unit, onFinished: () -> Unit) {
    val context = LocalContext.current
    var permitted by remember { mutableStateOf(ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) }
    var state by remember { mutableStateOf<PairingState>(PairingState.Scanning) }
    val permissionRequest = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { permitted = it }

    LaunchedEffect(state) {
        val approving = state as? PairingState.Approving ?: return@LaunchedEffect
        // Owner approval starts the server challenge. Stop after one minute and cancel on navigation.
        repeat(12) {
            val result = withContext(Dispatchers.IO) { PairingHost.finishApproved(context, approving.payload) }
            if (result == ImportResult.IMPORTED || result == ImportResult.UPDATED) {
                onFinished()
                return@LaunchedEffect
            }
            delay(5_000)
        }
        state = PairingState.Error("Pairing approval timed out. Confirm it on your desktop, then try again.")
    }

    BackHandler { PairingHost.cancel(context); onDismiss() }

    Column(
        Modifier.fillMaxSize().safeDrawingPadding().padding(24.dp).testTag("qr-pairing-screen"),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
            Text("Pair device", style = MaterialTheme.typography.headlineSmall)
            OutlinedButton(onClick = { PairingHost.cancel(context); onDismiss() }, modifier = Modifier.testTag("qr-cancel-button")) { Text(stringResource(R.string.peppy_cancel)) }
        }
        when (val current = state) {
            PairingState.Scanning -> if (permitted) {
                QRScanner(Modifier.fillMaxWidth().weight(1f)) { raw ->
                    val payload = PairingHost.parseQr(raw) ?: return@QRScanner
                    state = PairingState.Claiming(payload)
                }
                Text("Point at the QR code shown on your desktop")
            } else {
                Text("Camera access required", Modifier.testTag("qr-camera-permission"))
                Text("Peppy needs the camera to scan the pairing QR code shown on your desktop.")
                Button(onClick = { permissionRequest.launch(Manifest.permission.CAMERA) }) { Text("Allow camera access") }
                OutlinedButton(onClick = { context.startActivity(android.content.Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS).setData(android.net.Uri.parse("package:${context.packageName}"))) }) { Text("Open Settings") }
            }
            is PairingState.Claiming -> {
                PairingProgress("Claiming pairing intent…")
                LaunchedEffect(current.payload) {
                    val claim = withContext(Dispatchers.IO) { PairingHost.claim(context, current.payload) }
                    state = claim?.let { PairingState.Decoded(current.payload, it) }
                        ?: PairingState.Error("This pairing code could not be claimed. Generate a new code on your desktop.")
                }
            }
            is PairingState.Decoded -> {
                Text("Server: ${current.payload.origin}", Modifier.testTag("qr-server-origin"))
                Text(current.claim.sas, Modifier.testTag("qr-sas-code"), style = MaterialTheme.typography.headlineLarge)
                Text("Check this matches your desktop before approving")
                Button(onClick = { state = PairingState.Approving(current.payload) }, modifier = Modifier.testTag("qr-approve-button")) { Text("Approve pairing") }
            }
            is PairingState.Approving -> PairingProgress("Verifying with server…")
            is PairingState.Error -> {
                Text(current.message, Modifier.testTag("qr-error"), color = MaterialTheme.colorScheme.error)
                Button(onClick = { state = PairingState.Scanning }) { Text("Try again") }
            }
        }
    }
}

@Composable
private fun PairingProgress(message: String) {
    Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.testTag("qr-progress")) {
        CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
        Text(message)
    }
}
