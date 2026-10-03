package dev.peppy.mobile

import android.content.Context

/**
 * Local consent only. Policy, default account, books, scan checkpoints and edit state live in the
 * Rust core; this file never mirrors them.
 */
class ContactSyncPreferences(context: Context) {
    private val prefs = context.applicationContext.getSharedPreferences(NAME, Context.MODE_PRIVATE)

    /** The user's explicit opt-in to contact capture on this phone. */
    var enabled: Boolean
        get() = prefs.getBoolean(ENABLED, false)
        set(value) { prefs.edit().putBoolean(ENABLED, value).apply() }

    /**
     * Set when the user stopped and retired sync while the vault was locked; the next unlocked
     * pass publishes the retirement. Committed synchronously so a kill cannot lose the request.
     */
    var retirePending: Boolean
        get() = prefs.getBoolean(RETIRE_PENDING, false)
        set(value) { check(prefs.edit().putBoolean(RETIRE_PENDING, value).commit()) }

    private companion object {
        const val NAME = "peppy-contact-sync"
        const val ENABLED = "enabled"
        const val RETIRE_PENDING = "retire-pending"
    }
}

data class ContactAccount(val name: String, val type: String) {
    val label: String get() = "$type · $name"
}
