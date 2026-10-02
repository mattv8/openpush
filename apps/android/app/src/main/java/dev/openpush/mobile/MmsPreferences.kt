package dev.openpush.mobile

import android.content.Context
import java.util.UUID

/** Small, non-content settings for the local MMS acquisition loop. */
class MmsPreferences(context: Context) {
    private val prefs = context.applicationContext.getSharedPreferences(NAME, Context.MODE_PRIVATE)

    var enabled: Boolean
        get() = prefs.getBoolean(ENABLED, false)
        set(value) {
            val edit = prefs.edit().putBoolean(ENABLED, value)
            if (value && !enabled) edit.remove(historyBaselineKey())
            edit.apply()
        }

    var importHistory: Boolean
        get() = prefs.getBoolean(IMPORT_HISTORY, false)
        set(value) { prefs.edit().putBoolean(IMPORT_HISTORY, value).apply() }

    internal fun historyBaseline(): Long? = prefs.getLong(historyBaselineKey(), -1L).takeIf { it >= 0 }

    internal fun setHistoryBaseline(value: Long) {
        check(value >= 0)
        prefs.edit().putLong(historyBaselineKey(), value).apply()
    }

    internal fun completeHistoryImport() {
        prefs.edit().putBoolean(IMPORT_HISTORY, false).apply()
    }

    internal fun recheckCursor(): String? = prefs.getString(RECHECK_CURSOR, null)

    internal fun setRecheckCursor(value: String?) {
        prefs.edit().apply {
            if (value == null) remove(RECHECK_CURSOR) else putString(RECHECK_CURSOR, value)
        }.apply()
    }

    private fun historyBaselineKey() = "history-baseline:$sourceGeneration:${MmsCapture.PROVIDER_SCOPE}"

    /** Install identity, generated once and deliberately unrelated to message content or time. */
    val sourceGeneration: String = prefs.getString(SOURCE_GENERATION, null) ?: UUID.randomUUID().toString().also {
        check(prefs.edit().putString(SOURCE_GENERATION, it).commit())
    }

    companion object {
        private const val NAME = "openpush-mms-capture"
        private const val ENABLED = "enabled"
        private const val IMPORT_HISTORY = "import-history"
        private const val SOURCE_GENERATION = "source-generation"
        private const val RECHECK_CURSOR = "pending-recheck-cursor"
    }
}

data class MmsCaptureSummary(val captured: Int, val pending: Int, val unavailable: Int, val more: Boolean)
