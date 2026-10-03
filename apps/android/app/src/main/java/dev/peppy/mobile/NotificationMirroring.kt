package dev.peppy.mobile

import android.app.Notification
import android.app.NotificationManager
import android.content.Context
import android.os.Handler
import android.os.HandlerThread
import android.provider.Settings
import android.service.notification.NotificationListenerService
import android.service.notification.NotificationListenerService.Ranking
import android.service.notification.NotificationListenerService.RankingMap
import android.service.notification.StatusBarNotification
import androidx.core.app.NotificationManagerCompat
import uniffi.peppy_mobile_bindings.MobileBindingsException
import uniffi.peppy_mobile_bindings.NativeNotificationCapture
import uniffi.peppy_mobile_bindings.NativeNotificationCaptureOutcome
import uniffi.peppy_mobile_bindings.NativeClientInterface
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

/** Local Android capture choices. Core remains the durable synchronized filter authority. */
object NotificationMirrorPreferences {
    fun enabled(context: Context) = GatewayPolicyHost(context).mirroringEnabled
    fun setEnabled(context: Context, enabled: Boolean) { GatewayPolicyHost(context).mirroringEnabled = enabled }
}

/** Outcome surfaced by the explicit settings test action; no OS notification is fabricated. */
enum class NotificationTestCaptureResult { CAPTURED, ALREADY_QUEUED, LOCKED, POLICY_BLOCKED, ERROR }

/**
 * Writes one clearly marked synthetic record directly to core. It never represents an OS
 * notification, so its key/lifetime cannot match an application notification or be dismissed by
 * a remote dismissal request.
 */
internal object NotificationTestCapture {
    fun capture(
        client: NativeClientInterface?,
        permitted: Boolean,
        enqueue: () -> Unit,
        id: String = UUID.randomUUID().toString(),
    ): NotificationTestCaptureResult {
        if (!permitted) return NotificationTestCaptureResult.POLICY_BLOCKED
        if (client == null) return NotificationTestCaptureResult.LOCKED
        return try {
            // A settings test is bounded to one durable synthetic record. Repeated taps do not
            // create an unbounded history of fake notifications.
            if (client.notificationSnapshot().notifications.any { it.target.notificationKey == SYNTHETIC_KEY }) {
                return NotificationTestCaptureResult.ALREADY_QUEUED
            }
            val outcome = client.captureNotification(
                NativeNotificationCapture(
                    SYNTHETIC_KEY, "$SYNTHETIC_KEY:$id", "dev.peppy.mobile.synthetic",
                    "Peppy test", "Peppy test notification", "This is a safe test record.", "peppy_test",
                    System.currentTimeMillis(), false,
                ),
            )
            if (outcome == NativeNotificationCaptureOutcome.CAPTURED) {
                enqueue()
                NotificationTestCaptureResult.CAPTURED
            } else NotificationTestCaptureResult.ERROR
        } catch (_: MobileBindingsException) {
            NotificationTestCaptureResult.ERROR
        }
    }

    const val SYNTHETIC_KEY = "peppy-synthetic-test"
}

/** Synthetic records are core-only and are never inferred absent from Android's active list. */
internal object NotificationReconciliation {
    fun shouldRemoveAbsentSnapshot(key: String): Boolean = key != NotificationTestCapture.SYNTHETIC_KEY
}

data class ObservedNotificationApp(val packageName: String, val label: String)

object NotificationMirrorAccess {
    fun granted(context: Context): Boolean = NotificationManagerCompat.getEnabledListenerPackages(context).contains(context.packageName)
    fun settingsIntent() = android.content.Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS)
}

/** Pure extraction and exclusion policy; never forwards actions, intents, media, or extras wholesale. */
internal object NotificationExtraction {
    private const val MAX_BODY_BYTES = 4096
    private const val MAX_LABEL = 256

    fun removedInstanceStillActive(removed: StatusBarNotification, active: Array<StatusBarNotification>) =
        active.any { it.key == removed.key && it.postTime == removed.postTime }

    data class Captured(
        val key: String,
        val instance: String,
        val packageName: String,
        val appName: String,
        val title: String,
        val text: String,
        val category: String?,
        val postedAt: Long,
        val dismissible: Boolean,
        val isSilent: Boolean,
    )

    fun extract(context: Context, sbn: StatusBarNotification, isSilent: Boolean = false): Captured? {
        val notification = sbn.notification ?: return null
        if (sbn.packageName == context.packageName || sbn.packageName == defaultSmsPackage(context)) return null
        if (sbn.isOngoing || (notification.flags and Notification.FLAG_GROUP_SUMMARY) != 0) return null
        val extras = notification.extras ?: return null
        if (extras.getBoolean(Notification.EXTRA_PROGRESS_INDETERMINATE) || extras.getInt(Notification.EXTRA_PROGRESS_MAX, 0) > 0) return null
        val rawTitle = extras.getCharSequence(Notification.EXTRA_TITLE).safeText(512)
        val rawText = sequenceOf(Notification.EXTRA_BIG_TEXT, Notification.EXTRA_TEXT, Notification.EXTRA_SUB_TEXT)
            .map { extras.getCharSequence(it).safeText(MAX_BODY_BYTES) }.firstOrNull { it.isNotEmpty() }.orEmpty()
        val title = utf8Truncate(rawTitle, MAX_BODY_BYTES)
        val text = utf8Truncate(rawText, MAX_BODY_BYTES - title.toByteArray(Charsets.UTF_8).size)
        if (title.isEmpty() && text.isEmpty()) return null
        val label = try {
            context.packageManager.getApplicationLabel(context.packageManager.getApplicationInfo(sbn.packageName, 0)).safeText(MAX_LABEL)
        } catch (_: Exception) { sbn.packageName.take(MAX_LABEL) }
        // Android's key plus post time distinguishes ordinary remove/repost lifetimes where possible.
        return Captured(sbn.key, "${sbn.key}:${sbn.postTime}", sbn.packageName, label, title, text,
            notification.category, sbn.postTime, sbn.isClearable, isSilent)
    }

    private fun defaultSmsPackage(context: Context): String? = try { android.provider.Telephony.Sms.getDefaultSmsPackage(context) } catch (_: Exception) { null }
    private fun CharSequence?.safeText(max: Int): String = utf8Truncate(this?.toString()?.trim().orEmpty(), max)
    internal fun utf8Truncate(value: String, maxBytes: Int): String {
        val output = StringBuilder(); var used = 0; var index = 0
        while (index < value.length) {
            val c = value[index]
            if (c.isHighSurrogate() && (index + 1 == value.length || !value[index + 1].isLowSurrogate())) { index++; continue }
            if (c.isLowSurrogate()) { index++; continue }
            val point = Character.codePointAt(value, index); val part = String(Character.toChars(point)); val bytes = part.toByteArray(Charsets.UTF_8)
            if (used + bytes.size > maxBytes) break
            output.append(part); used += bytes.size; index += Character.charCount(point)
        }
        return output.toString()
    }
}

/** User-granted listener; a short in-memory coalescer prevents update storms reaching durable core storage. */
class NotificationMirrorService : NotificationListenerService() {
    private val worker by lazy { HandlerThread("peppy-notification-listener").apply { start() } }
    private val handler by lazy { Handler(worker.looper) }
    private val pending = ConcurrentHashMap<String, NotificationExtraction.Captured>()
    private val callbacks = ConcurrentHashMap<String, Runnable>()
    private val observed = ConcurrentHashMap<String, ObservedNotificationApp>()

    override fun onNotificationPosted(sbn: StatusBarNotification) {
        handler.post { capturePosted(sbn, null) }
    }

    override fun onNotificationPosted(sbn: StatusBarNotification, rankingMap: RankingMap) {
        handler.post { capturePosted(sbn, rankingMap) }
    }

    private fun capturePosted(sbn: StatusBarNotification, rankingMap: RankingMap?) {
        val capture = NotificationExtraction.extract(applicationContext, sbn, isSilent(sbn, rankingMap)) ?: return
        observed[capture.packageName] = ObservedNotificationApp(capture.packageName, capture.appName)
        if (!permitted(capture.isSilent)) return
        pending[capture.key] = capture
        callbacks.remove(capture.key)?.let(handler::removeCallbacks)
        Runnable { capturePending(capture.key, capture.instance) }.also { callbacks[capture.key] = it; handler.postDelayed(it, COALESCE_MS) }
    }

    override fun onNotificationRemoved(sbn: StatusBarNotification) {
        handler.post { captureRemoved(sbn) }
    }

    private fun captureRemoved(sbn: StatusBarNotification) {
        val key = sbn.key
        if (!NotificationReconciliation.shouldRemoveAbsentSnapshot(key)) return
        val instance = "${key}:${sbn.postTime}"
        // A delayed removal callback must not retire a repost that is currently active.
        val active = try { activeNotifications ?: return } catch (_: SecurityException) { return }
        if (NotificationExtraction.removedInstanceStillActive(sbn, active)) return
        pending.remove(key)
        callbacks.remove(key)?.let(handler::removeCallbacks)
        // No plaintext removal backlog is retained when the vault is locked.
        val session = NativeGateway.session(applicationContext) ?: return
        try { session.client.removeNotification(key, instance) } catch (_: MobileBindingsException) { return }
        GatewayWork.enqueue(applicationContext)
    }

    private fun capturePending(key: String, instance: String) {
        val capture = pending[key]?.takeIf { it.instance == instance } ?: return
        pending.remove(key, capture); callbacks.remove(key)
        if (!permitted(capture.isSilent)) return
        val active = try { activeNotifications ?: return } catch (_: SecurityException) { return }
        if (active.none { it.key == key && "${it.key}:${it.postTime}" == instance }) return
        val session = NativeGateway.session(applicationContext) ?: return
        try {
            val outcome = session.client.captureNotification(NativeNotificationCapture(
                capture.key, capture.instance, capture.packageName, capture.appName, capture.title, capture.text,
                capture.category, capture.postedAt, capture.dismissible,
            ))
            if (outcome == NativeNotificationCaptureOutcome.CAPTURED) GatewayWork.enqueue(applicationContext)
        } catch (_: MobileBindingsException) {
            // Core rejects duplicates/locked/filtered input without a host-side plaintext retry queue.
        }
    }

    private fun permitted(isSilent: Boolean): Boolean {
        if (connected !== this) return false
        // This calls Rust at effect time with the listener, Wi-Fi transport, and RankingMap facts.
        return GatewayPolicyHost(this).decision(notificationIsSilent = isSilent)?.captureNotification == true &&
            NativeGateway.session(applicationContext) != null
    }

    internal fun observedApps(): List<ObservedNotificationApp> = observed.values.sortedBy { it.label.lowercase() }

    internal fun applyDismissals(): Int {
        if (!NotificationMirrorAccess.granted(this)) return 0
        val session = NativeGateway.session(applicationContext) ?: return 0
        var completed = 0
        val active = try { activeNotifications ?: return 0 } catch (_: SecurityException) { return 0 }
        for (dismissal in try { session.client.pendingNotificationDismissals(50uL) } catch (_: MobileBindingsException) { return 0 }) {
            val current = active.firstOrNull { it.key == dismissal.target.notificationKey }
            if (current == null || "${current.key}:${current.postTime}" != dismissal.instance) continue
            if (!current.isClearable) continue
            try {
                cancelNotification(current.key)
                session.client.completeNotificationDismissal(dismissal.id)
                completed++
            } catch (_: SecurityException) { return completed } catch (_: MobileBindingsException) { return completed }
        }
        return completed
    }

    private fun clearPending() { pending.clear(); callbacks.values.forEach(handler::removeCallbacks); callbacks.clear() }
    override fun onDestroy() { if (connected === this) connected = null; clearPending(); worker.quitSafely(); super.onDestroy() }

    companion object {
        private const val COALESCE_MS = 2_000L
        @Volatile private var connected: NotificationMirrorService? = null
        fun runPendingDismissals(): Int = connected?.applyDismissals() ?: 0
        fun observedApps(): List<ObservedNotificationApp> = connected?.observedApps().orEmpty()
        fun reconcile() { connected?.reconcile() }
        fun clearPendingCaptures() { connected?.clearPending() }
        fun captureTest(context: Context): NotificationTestCaptureResult {
            val host = GatewayPolicyHost(context)
            val permitted = host.decision(notificationIsSilent = false)?.captureNotification == true
            return NotificationTestCapture.capture(
                client = NativeGateway.session(context)?.client,
                permitted = permitted,
                enqueue = { GatewayWork.enqueue(context) },
            )
        }
    }

    private fun reconcile() {
        handler.post {
            if (connected !== this) return@post
            // Reconciliation includes removals, so it must observe the same effect-time Wi-Fi
            // and master policy as capture rather than emitting traffic while disabled.
            if (GatewayPolicyHost(this).decision(notificationIsSilent = false)?.captureNotification != true) return@post
            val active = try { activeNotifications ?: return@post } catch (_: SecurityException) { return@post }
            val session = NativeGateway.session(applicationContext) ?: return@post
            val rankings = currentRanking
            val eligibleKeys = active.filter { capture ->
                NotificationExtraction.extract(this, capture, isSilent(capture, rankings)) != null &&
                    permitted(isSilent(capture, rankings))
            }.map { it.key }.toSet()
            try {
                val source = session.client.notificationSourceDeviceId()
                val absent = session.client.notificationSnapshot().notifications.filter {
                    it.target.sourceDeviceId == source && it.target.notificationKey !in eligibleKeys &&
                        NotificationReconciliation.shouldRemoveAbsentSnapshot(it.target.notificationKey)
                }
                absent.forEach { session.client.removeNotification(it.target.notificationKey, "reconcile") }
                if (absent.isNotEmpty()) GatewayWork.enqueue(applicationContext)
            } catch (_: MobileBindingsException) { return@post }
            active.forEach { capturePosted(it, rankings) }
        }
    }

    private fun isSilent(sbn: StatusBarNotification, rankings: RankingMap?): Boolean {
        val ranking = Ranking()
        return rankings?.getRanking(sbn.key, ranking) == true && ranking.importance <= NotificationManager.IMPORTANCE_LOW
    }
    override fun onListenerDisconnected() { clearPending(); connected = null; super.onListenerDisconnected() }
    override fun onListenerConnected() { connected = this; super.onListenerConnected(); reconcile() }
}
