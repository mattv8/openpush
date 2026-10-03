package dev.openpush.mobile

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.BatteryManager
import androidx.core.content.ContextCompat
import uniffi.openpush_mobile_bindings.NativeClientInterface

/** What the settings screen shows; every book/policy/request value comes from the core. */
internal data class ContactSettingsState(
    val enabled: Boolean,
    val readGranted: Boolean,
    val writeGranted: Boolean,
    /** False while not enrolled or the vault keys are locked. */
    val unlocked: Boolean,
    /** False until the core has a usable compaction key and completed its frontier backfill. */
    val contactsReady: Boolean,
    val needsUnlock: Boolean,
    val backfillPending: Boolean,
    /** The server is supported but not every roster device has declared snapshot fencing yet. */
    val inactiveRoster: Boolean,
    val retirePending: Boolean,
    val overview: ContactOverview?,
)

/**
 * Android host glue for contact sync: runs one bounded coordinator pass inside the existing
 * gateway sync pass (after receive/apply, before media and outbox upload), keeps the
 * process-lifetime change observer registered while sync is enabled, and serves the settings UI.
 * No foreground service, no plaintext queue: a locked vault simply skips (the OS remains the
 * source and is re-read later).
 */
internal object ContactSyncHost {
    private val lock = Any()
    private var observer: ContactChangeObserver? = null

    fun access(context: Context) = ContactsAccess(
        read = granted(context, Manifest.permission.READ_CONTACTS),
        write = granted(context, Manifest.permission.WRITE_CONTACTS),
    )

    private fun granted(context: Context, permission: String) =
        ContextCompat.checkSelfPermission(context, permission) == PackageManager.PERMISSION_GRANTED

    private data class ContactReadiness(
        val contactsReady: Boolean,
        val needsUnlock: Boolean,
        val backfillPending: Boolean,
        val inactiveRoster: Boolean,
    )

    private fun readiness(client: NativeClientInterface): ContactReadiness {
        val value = org.json.JSONObject(client.contactSyncReadinessJson())
        fun flag(name: String) = value.opt(name) as? Boolean ?: throw org.json.JSONException(name)
        val supported = flag("server_supported")
        return ContactReadiness(
            contactsReady = flag("contacts_ready"),
            needsUnlock = flag("needs_unlock"),
            backfillPending = value.optString("state") == "backfill_pending",
            inactiveRoster = supported && !flag("server_active"),
        )
    }

    fun coordinator(context: Context, client: NativeClientInterface, deviceId: String) = ContactSyncCoordinator(
        core = NativeContactCore(client),
        os = AndroidContactsOs(AndroidContactsProvider(context)),
        deviceId = deviceId,
        access = { access(context) },
        photoTempDirectory = java.io.File(context.noBackupFilesDir, "contact-photo-tmp"),
        deviceName = android.os.Build.MODEL,
        region = regionHint(context),
    )

    /** Explicit SIM country first, then the device locale; null when neither is a 2-letter code. */
    fun regionHint(context: Context): String? {
        val sim = runCatching {
            context.getSystemService(android.telephony.TelephonyManager::class.java)?.simCountryIso
        }.getOrNull()
        return listOf(sim, java.util.Locale.getDefault().country)
            .firstOrNull { it != null && it.length == 2 && it.all(Char::isLetter) }?.uppercase()
    }

    /**
     * One pass for the gateway worker. Contact work starts only after core readiness has a
     * compaction key and frontier backfill; inactive roster compaction does not block it.
     * Returns true when bounded backfill work remains.
     */
    fun pass(context: Context, session: GatewaySession): Boolean {
        val prefs = ContactSyncPreferences(context)
        ensureObserver(context)
        if (!prefs.enabled && !prefs.retirePending) return false
        val readiness = readiness(session.client)
        if (!readiness.contactsReady) return readiness.backfillPending
        // GatewaySync performs the fenced repair before invoking this callback. Keep this guard
        // here too so no alternate host caller can capture or apply contacts while it is held.
        if (session.client.contactRepairRequired()) return false
        val coordinator = coordinator(context, session.client, session.deviceId)
        if (prefs.retirePending) {
            coordinator.retire()
            prefs.retirePending = false
            return false
        }
        return coordinator.run(charging = charging(context)).more
    }

    /** Process-lifetime observer while enabled and readable; changes enqueue the unique gateway pass. */
    fun ensureObserver(context: Context) {
        val app = context.applicationContext
        val wanted = ContactSyncPreferences(app).enabled && access(app).read
        synchronized(lock) {
            val current = observer
            if (wanted && current == null) {
                observer = ContactChangeObserver(app.contentResolver, { GatewayScheduler.schedule(app) }).also { it.start() }
            } else if (!wanted && current != null) {
                current.stop()
                observer = null
            }
        }
    }

    private fun charging(context: Context): Boolean =
        context.getSystemService(BatteryManager::class.java)?.isCharging == true

    /** Settings state; performs core and provider I/O, so call off the main thread. */
    fun settingsState(context: Context): ContactSettingsState {
        val prefs = ContactSyncPreferences(context)
        val granted = access(context)
        val session = NativeGateway.session(context)
        val overview = session?.let { runCatching { coordinator(context, it.client, it.deviceId).overview() }.getOrNull() }
        val readiness = session?.let { runCatching { readiness(it.client) }.getOrNull() }
        return ContactSettingsState(
            enabled = prefs.enabled,
            readGranted = granted.read,
            writeGranted = granted.write,
            unlocked = session != null,
            contactsReady = readiness?.contactsReady ?: false,
            needsUnlock = readiness?.needsUnlock ?: false,
            backfillPending = readiness?.backfillPending ?: false,
            inactiveRoster = readiness?.inactiveRoster ?: false,
            retirePending = prefs.retirePending,
            overview = overview,
        )
    }

    /** Runs a core-only settings action with the unlocked session; false when locked. */
    fun withCoordinator(context: Context, action: (ContactSyncCoordinator) -> Unit): Boolean {
        val session = NativeGateway.session(context) ?: return false
        action(coordinator(context, session.client, session.deviceId))
        GatewayScheduler.schedule(context)
        return true
    }

    /**
     * Stop and retire: disables capture immediately and publishes the owner book as retired now
     * (or on the next unlocked pass). OS contacts are never deleted.
     */
    fun stopAndRetire(context: Context) {
        val prefs = ContactSyncPreferences(context)
        prefs.retirePending = true
        prefs.enabled = false
        ensureObserver(context)
        val session = NativeGateway.session(context)
        if (session != null) {
            coordinator(context, session.client, session.deviceId).retire()
            prefs.retirePending = false
        }
        GatewayScheduler.schedule(context)
    }
}
