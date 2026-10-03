package dev.openpush.mobile

import android.content.ContentResolver
import android.database.ContentObserver
import android.os.Handler
import android.os.Looper
import android.provider.ContactsContract

/**
 * Process-lifetime observer; callers register from the unlocked host and unregister on shutdown.
 * Changes are debounced by [DEBOUNCE_MS], but a continuous stream still dispatches within
 * [MAX_LATENCY_MS] of its first change. [onDebouncedChange] runs on [handler] and must only enqueue
 * bounded background work, never scan the provider itself.
 */
internal class ContactChangeObserver(
    private val resolver: ContentResolver,
    private val onDebouncedChange: () -> Unit,
    private val handler: Handler = Handler(Looper.getMainLooper()),
) : ContentObserver(handler) {
    private var firstPendingAt = 0L
    private val dispatch = Runnable { firstPendingAt = 0L; onDebouncedChange() }

    fun start() = resolver.registerContentObserver(ContactsContract.Contacts.CONTENT_URI, true, this)
    fun stop() { handler.removeCallbacks(dispatch); firstPendingAt = 0L; resolver.unregisterContentObserver(this) }
    override fun onChange(selfChange: Boolean) { onChange(selfChange, null) }
    override fun onChange(selfChange: Boolean, uri: android.net.Uri?) {
        val now = android.os.SystemClock.uptimeMillis()
        if (firstPendingAt == 0L) firstPendingAt = now
        handler.removeCallbacks(dispatch)
        handler.postDelayed(dispatch, minOf(DEBOUNCE_MS, (MAX_LATENCY_MS - (now - firstPendingAt)).coerceAtLeast(0)))
    }

    companion object { const val DEBOUNCE_MS = 45_000L; const val MAX_LATENCY_MS = 5 * 60_000L }
}
