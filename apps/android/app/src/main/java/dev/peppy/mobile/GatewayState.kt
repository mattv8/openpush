package dev.peppy.mobile

import android.content.Context

/**
 * Durable host-side sync phase. A fresh database never replays live history: it bootstraps from a
 * snapshot (whose commands the core imports as historical), and only after that decision may it
 * upload or touch the carrier.
 */
enum class SyncPhase {
    /** Fresh enrollment: no live replay, upload, or carrier work until the snapshot is applied. */
    BOOTSTRAP,
    /** Bootstrap snapshot published; draining until `snapshotRemaining == 0`. */
    BOOTSTRAP_DRAINING,
    LIVE,
    /** Terminal until re-enrollment as a new device: no upload, no carrier work. */
    RECOVERY_REQUIRED,
}

enum class RecoveryReason {
    /** The server already holds records produced by this device ID (earlier install / copied credential). */
    REUSED_DEVICE_IDENTITY,
    /** The server rejected an upload as a producer idempotency conflict. */
    PRODUCER_CONFLICT,
    /**
     * The local database file disappeared after it had been initialized. Recreating it would lose
     * the attempt ledger and could re-execute live commands, so it is never recreated.
     */
    DATABASE_MISSING,
}

class GatewayStateStore(context: Context) {
    private val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /** Absent state is treated as a fresh bootstrap, never as live. */
    val phase: SyncPhase
        get() = prefs.getString(P_PHASE, null)?.let { name -> SyncPhase.entries.firstOrNull { it.name == name } } ?: SyncPhase.BOOTSTRAP

    val recoveryReason: RecoveryReason?
        get() = prefs.getString(P_RECOVERY, null)?.let { name -> RecoveryReason.entries.firstOrNull { it.name == name } }

    /** Server error code of a permanently rejected outbox envelope; uploads and carrier work stop. */
    val outboxRejection: String?
        get() = prefs.getString(P_OUTBOX_REJECTION, null)

    fun startBootstrap() = commit { clear().putString(P_PHASE, SyncPhase.BOOTSTRAP.name) }

    /** Drops state belonging to an archived enrollment before a different device is imported. */
    fun resetForNewEnrollment() = startBootstrap()

    fun advance(to: SyncPhase) = synchronized(LOCK) {
        // RECOVERY_REQUIRED can only be left by a new enrollment (startBootstrap on a new database).
        if (phase != SyncPhase.RECOVERY_REQUIRED) commit { putString(P_PHASE, to.name) }
    }

    fun requireRecovery(reason: RecoveryReason) = commit {
        putString(P_PHASE, SyncPhase.RECOVERY_REQUIRED.name).putString(P_RECOVERY, reason.name)
    }

    fun rejectOutbox(code: String) = commit { putString(P_OUTBOX_REJECTION, code.take(64)) }

    private inline fun commit(edit: android.content.SharedPreferences.Editor.() -> android.content.SharedPreferences.Editor) {
        synchronized(LOCK) { check(prefs.edit().edit().commit()) { "gateway state not persisted" } }
    }

    private companion object {
        const val PREFS = "peppy-gateway-state"
        const val P_PHASE = "phase.v1"
        const val P_RECOVERY = "recovery.v1"
        const val P_OUTBOX_REJECTION = "outbox-rejection.v1"
        val LOCK = Any()
    }
}
