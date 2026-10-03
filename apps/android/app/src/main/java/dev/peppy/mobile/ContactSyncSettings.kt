package dev.peppy.mobile

import android.Manifest
import android.content.Context
import android.text.format.DateUtils
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

internal val CONTACT_PERMISSIONS = arrayOf(Manifest.permission.READ_CONTACTS, Manifest.permission.WRITE_CONTACTS)

/**
 * Contact sync settings. Consent is local; everything else (book state, policy, default account,
 * pending approvals, uncertain writes, last full scan) is read from and written to the core.
 * Nothing here scans or writes OS contacts.
 */
@Composable
internal fun ContactSyncSettings(
    context: Context,
    refresh: Int,
    requestPermissions: () -> Unit,
    onChanged: () -> Unit,
) {
    val scope = rememberCoroutineScope()
    var reload by remember { mutableIntStateOf(0) }
    var busy by remember { mutableStateOf(false) }
    var retireConfirm by remember { mutableStateOf(false) }
    val state by produceState<ContactSettingsState?>(null, refresh, reload) {
        value = withContext(Dispatchers.IO) { runCatching { ContactSyncHost.settingsState(context) }.getOrNull() }
    }
    fun act(block: () -> Unit) {
        busy = true
        scope.launch {
            withContext(Dispatchers.IO) { runCatching(block) }
            busy = false
            reload++
            onChanged()
        }
    }

    Section("contact-sync-section", "Contact sync") {
        val current = state
        val enabled = current?.enabled == true
        Row(Modifier.fillMaxWidth().testTag("contact-sync-enable-row"), horizontalArrangement = Arrangement.SpaceBetween) {
            Text("Sync contacts", style = MaterialTheme.typography.bodyMedium)
            Switch(
                checked = enabled,
                enabled = current != null && !busy,
                onCheckedChange = { value ->
                    act {
                        ContactSyncPreferences(context).enabled = value
                        ContactSyncHost.ensureObserver(context)
                        if (value) GatewayScheduler.schedule(context)
                    }
                    if (value && (current?.readGranted != true || current?.writeGranted != true)) requestPermissions()
                },
                modifier = Modifier.testTag("contact-sync-toggle"),
            )
        }
        Text(
            "When enabled, this phone's contacts sync end-to-end encrypted. Desktop can browse, search and request edits; " +
                "this phone applies them. Contacts with shared numbers remain distinct records. Sync can take hours while the phone is idle.",
            style = MaterialTheme.typography.bodySmall, modifier = Modifier.testTag("contact-sync-description"),
        )
        if (current == null) return@Section
        StatusRow(
            "contact-access-status", "Contacts access",
            when {
                !enabled -> "Disabled"
                current.readGranted && current.writeGranted -> "Full access"
                current.readGranted -> "Read only — desktop edits are refused"
                else -> "Permission denied — tap to grant"
            },
        )
        if (enabled && (!current.readGranted || !current.writeGranted)) {
            OutlinedButton(requestPermissions, Modifier.testTag("contact-grant-button")) { Text("Grant contacts access") }
        }
        if (enabled && !current.unlocked) {
            Text("Unlock the vault above to sync contacts.", style = MaterialTheme.typography.bodySmall, modifier = Modifier.testTag("contact-vault-status"))
        } else if (enabled && current.needsUnlock) {
            Text(
                "Unlock with your passphrase to finish preparing contact sync.",
                style = MaterialTheme.typography.bodySmall, modifier = Modifier.testTag("contact-server-status"),
            )
        } else if (enabled && current.backfillPending) {
            Text(
                "Preparing existing sync history for contacts. This continues in the background.",
                style = MaterialTheme.typography.bodySmall, modifier = Modifier.testTag("contact-server-status"),
            )
        } else if (enabled && !current.contactsReady) {
            Text(
                "Waiting for a compatible server. Contacts sync only after the server supports compaction.",
                style = MaterialTheme.typography.bodySmall, modifier = Modifier.testTag("contact-server-status"),
            )
        } else if (enabled && current.inactiveRoster) {
            Text(
                "Some devices have not upgraded yet. Contact sync is available; history retention is paused.",
                style = MaterialTheme.typography.bodySmall, modifier = Modifier.testTag("contact-server-status"),
            )
        }
        if (current.retirePending) {
            Text("Retirement will be published the next time the vault is unlocked.", style = MaterialTheme.typography.bodySmall, modifier = Modifier.testTag("contact-retire-pending"))
        }
        val overview = current.overview ?: return@Section
        if (overview.bookId == null) {
            if (enabled) Text("Waiting for the first contact scan.", style = MaterialTheme.typography.bodySmall, modifier = Modifier.testTag("contact-sync-status"))
            return@Section
        }
        StatusRow(
            "contact-sync-status", "Last full scan",
            overview.lastFullScanAt?.let { DateUtils.getRelativeTimeSpanString(it).toString() } ?: "Not yet",
        )
        overview.contactCount?.let { StatusRow("contact-sync-count", "Contacts synced", it.toString()) }

        val writable = overview.accounts.filter { it.writable }
        if (writable.isNotEmpty()) {
            Text("Save new contacts to", style = MaterialTheme.typography.labelMedium)
            Column(Modifier.testTag("contact-account-list"), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                writable.forEach { account ->
                    val isSelected = overview.defaultAccountId == account.id
                    OutlinedButton(
                        { act { ContactSyncHost.withCoordinator(context) { it.setDefaultAccount(account.id) } } },
                        Modifier.semantics { selected = isSelected }.testTag("contact-account-${account.id}"),
                        enabled = !busy,
                    ) { Text((if (isSelected) "✓ " else "") + account.name) }
                }
            }
        }

        Text("Desktop edits", style = MaterialTheme.typography.labelMedium)
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.testTag("remote-edits-policy")) {
            listOf("auto" to "Auto", "confirm" to "Confirm", "off" to "Off").forEach { (mode, label) ->
                val isSelected = overview.remoteEdits == mode
                OutlinedButton(
                    { act { ContactSyncHost.withCoordinator(context) { it.setRemoteEdits(mode) } } },
                    Modifier.semantics { selected = isSelected }.testTag("remote-edits-$mode"),
                    enabled = !busy,
                ) { Text((if (isSelected) "✓ " else "") + label) }
            }
        }
        Text(
            when (overview.remoteEdits) {
                "auto" -> "Small changes apply automatically. Deleting more than 10 contacts or 5% in 24 hours asks for approval here."
                "off" -> "Desktop edits are not accepted."
                else -> "Every desktop edit waits for approval on this phone."
            },
            style = MaterialTheme.typography.bodySmall,
        )

        if (overview.pending.isNotEmpty()) {
            Text("Pending on this phone", style = MaterialTheme.typography.labelMedium)
            Column(Modifier.testTag("contact-pending-list"), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                overview.pending.forEach { item -> PendingRow(item, busy, context, ::act) }
            }
        }

        if (!retireConfirm) {
            OutlinedButton({ retireConfirm = true }, Modifier.testTag("contact-retire-button"), enabled = !busy) { Text("Stop syncing") }
        } else {
            Column(Modifier.testTag("contact-retire-confirmation"), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    "This stops contact sync and tells your other devices this book is retired. Contacts on this phone are not changed or deleted.",
                    style = MaterialTheme.typography.bodySmall,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedButton({ retireConfirm = false }, Modifier.testTag("contact-retire-cancel")) { Text("Keep syncing") }
                    Button({ retireConfirm = false; act { ContactSyncHost.stopAndRetire(context) } }, Modifier.testTag("contact-retire-confirm"), enabled = !busy) {
                        Text("Stop and retire")
                    }
                }
            }
        }
    }
}

@Composable
private fun PendingRow(item: ContactPendingItem, busy: Boolean, context: Context, act: (() -> Unit) -> Unit) {
    val tag = if (item.isHold) "contact-hold-${item.id}" else "contact-request-${item.id}"
    Column(Modifier.fillMaxWidth().testTag(tag), verticalArrangement = Arrangement.spacedBy(4.dp)) {
        val description = when {
            item.isHold -> "${item.count ?: 0} contacts disappeared from this phone. Remove them from your other devices?"
            item.state == "outcome_unknown" -> "This phone could not confirm a desktop ${item.kind} of ${item.displayName ?: "a contact"}. Check the contact, then dismiss."
            else -> "Desktop ${item.kind}: ${item.displayName ?: "contact"}" + item.fieldPaths.takeIf { it.isNotEmpty() }?.let { " (${it.joinToString()})" }.orEmpty()
        }
        Text(description, style = MaterialTheme.typography.bodySmall)
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            if (item.state == "outcome_unknown") {
                OutlinedButton({ act { ContactSyncHost.withCoordinator(context) { it.resolveUnknown(item.id) } } }, Modifier.testTag("$tag-resolve"), enabled = !busy) {
                    Text("Dismiss")
                }
            } else if (item.isHold || item.state in setOf("requested", "awaiting_approval")) {
                Button({ act { ContactSyncHost.withCoordinator(context) { it.decide(item, approve = true) } } }, Modifier.testTag("$tag-approve"), enabled = !busy) {
                    Text("Approve")
                }
                OutlinedButton({ act { ContactSyncHost.withCoordinator(context) { it.decide(item, approve = false) } } }, Modifier.testTag("$tag-reject"), enabled = !busy) {
                    Text("Reject")
                }
            }
        }
    }
}
