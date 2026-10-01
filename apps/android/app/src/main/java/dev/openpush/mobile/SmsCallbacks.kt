package dev.openpush.mobile

import android.app.Activity
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.telephony.SmsManager
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject
import uniffi.openpush_mobile_bindings.MobileBindingsException
import uniffi.openpush_mobile_bindings.NativeClientInterface
import uniffi.openpush_mobile_bindings.NativeSendResult

/**
 * Per-part carrier evidence. [PENDING] is a temporary delivery status that a later report for the
 * same part may replace; [UNKNOWN] is anything the platform does not document as proof.
 */
enum class PartOutcome { OK, FAILED, PENDING, UNKNOWN }

object SmsOutcomes {
    /**
     * SmsManager errors that are raised before the PDU is handed to the network. Generic failure
     * is deliberately absent: it can also mean a lost network acknowledgment, so it is UNKNOWN.
     */
    private val NOT_SENT_CODES = setOf(
        SmsManager.RESULT_ERROR_RADIO_OFF,
        SmsManager.RESULT_ERROR_NULL_PDU,
        SmsManager.RESULT_ERROR_NO_SERVICE,
        SmsManager.RESULT_ERROR_LIMIT_EXCEEDED,
        SmsManager.RESULT_ERROR_FDN_CHECK_FAILURE,
        SmsManager.RESULT_ERROR_SHORT_CODE_NOT_ALLOWED,
        SmsManager.RESULT_ERROR_SHORT_CODE_NEVER_ALLOWED,
    )

    fun sentPart(resultCode: Int): PartOutcome = when (resultCode) {
        Activity.RESULT_OK -> PartOutcome.OK
        in NOT_SENT_CODES -> PartOutcome.FAILED
        else -> PartOutcome.UNKNOWN
    }

    /**
     * 3GPP TP-Status (23.040 §9.2.3.15): 0x00..0x1F completed; 0x20..0x3F temporary, SC still
     * trying; 0x40..0x7F permanent or given up. Only "completed" is proof of delivery.
     */
    fun deliveredPart(format: String?, tpStatus: Int?): PartOutcome = when {
        format != "3gpp" || tpStatus == null -> PartOutcome.UNKNOWN
        tpStatus in 0x00..0x1F -> PartOutcome.OK
        tpStatus in 0x20..0x3F -> PartOutcome.PENDING
        tpStatus in 0x40..0x7F -> PartOutcome.FAILED
        else -> PartOutcome.UNKNOWN
    }

    /**
     * Conservative aggregation once every part has reported: all OK → SENT; all documented
     * pre-network FAILED → FAILED_CONFIRMED; any mix or any UNKNOWN → no claim (core keeps
     * SUBMITTED). A partially transmitted message is never reported as failed or sent.
     */
    fun aggregateSent(parts: List<PartOutcome?>): NativeSendResult? = when {
        parts.isEmpty() || parts.any { it == null } -> null
        parts.all { it == PartOutcome.OK } -> NativeSendResult.SENT
        parts.all { it == PartOutcome.FAILED } -> NativeSendResult.FAILED_CONFIRMED
        else -> null
    }

    fun aggregateDelivered(parts: List<PartOutcome?>): NativeSendResult? =
        if (parts.isNotEmpty() && parts.all { it == PartOutcome.OK }) NativeSendResult.DELIVERED else null
}

enum class CallbackKind(val segment: String) { SENT("sent"), DELIVERED("delivered") }

/** Identity of one carrier callback, carried only in the immutable data URI of its PendingIntent. */
data class CallbackIdentity(val kind: CallbackKind, val commandId: String, val part: Int, val parts: Int) {
    fun uri(): Uri = Uri.Builder().scheme(SCHEME).authority(AUTHORITY)
        .appendPath(kind.segment).appendPath(commandId).appendPath(part.toString()).appendPath(parts.toString()).build()

    companion object {
        const val SCHEME = "openpush-sms"
        const val AUTHORITY = "callback"
        private val COMMAND_ID = Regex("[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
        private val DECIMAL = Regex("0|[1-9][0-9]{0,2}")
        const val MAX_PARTS = 255

        fun parse(uri: Uri?): CallbackIdentity? {
            if (uri == null || uri.scheme != SCHEME || uri.authority != AUTHORITY || uri.query != null || uri.fragment != null) return null
            val segments = uri.pathSegments
            if (segments.size != 4 || !DECIMAL.matches(segments[2]) || !DECIMAL.matches(segments[3])) return null
            val kind = CallbackKind.entries.firstOrNull { it.segment == segments[0] } ?: return null
            val parts = segments[3].toInt().takeIf { it in 1..MAX_PARTS } ?: return null
            val part = segments[2].toInt().takeIf { it in 0 until parts } ?: return null
            return CallbackIdentity(kind, segments[1].takeIf(COMMAND_ID::matches) ?: return null, part, parts)
        }
    }
}

object SmsCallbackIntents {
    /**
     * Explicit-component broadcasts with a unique data URI per command/part/kind, so no two
     * callbacks can ever collapse into one PendingIntent. The intent is mutable only so telephony
     * can attach the status-report PDU; component, action and data cannot be filled in. Sent
     * callbacks fire once (ONE_SHOT); delivery callbacks may fire repeatedly (temporary, then final).
     */
    fun create(context: Context, identity: CallbackIdentity): PendingIntent {
        val intent = Intent(context, SmsStatusReceiver::class.java)
            .setAction(SmsStatusReceiver.ACTION)
            .setData(identity.uri())
        val mutable = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) PendingIntent.FLAG_MUTABLE else 0
        val oneShot = if (identity.kind == CallbackKind.SENT) PendingIntent.FLAG_ONE_SHOT else 0
        return PendingIntent.getBroadcast(context, 0, intent, oneShot or mutable)
    }
}

/**
 * Crash-safe aggregation of per-part carrier callbacks. Stores only command IDs, part outcomes,
 * and aggregate results still owed to the core, never message content or addresses. An aggregate
 * is persisted as "owed" before the core write and cleared only after it, so a crash in between
 * is replayed by [reconcile].
 */
class SmsCallbackStore(context: Context, private val now: () -> Long = System::currentTimeMillis) {
    private val prefs = context.getSharedPreferences("openpush-sms-callbacks", Context.MODE_PRIVATE)

    /** Must be called before the carrier call so callbacks after process death can aggregate. */
    fun register(commandId: String, parts: Int) = synchronized(LOCK) {
        require(parts in 1..CallbackIdentity.MAX_PARTS)
        val record = JSONObject().put("parts", parts).put("created", now())
            .put(SENT, JSONArray(List(parts) { JSONObject.NULL }))
            .put(DELIVERED, JSONArray(List(parts) { JSONObject.NULL }))
        check(prefs.edit().putString(key(commandId), record.toString()).commit())
    }

    /**
     * Records one part's outcome. Returns a newly final aggregate (also persisted as owed to the
     * core), or null. Final part outcomes are immutable; PENDING may be superseded.
     */
    fun record(identity: CallbackIdentity, outcome: PartOutcome): NativeSendResult? = synchronized(LOCK) {
        val record = load(identity.commandId) ?: return null
        if (record.optInt("parts") != identity.parts) return null
        val field = if (identity.kind == CallbackKind.SENT) SENT else DELIVERED
        val array = record.getJSONArray(field)
        val previous = outcomeAt(array, identity.part)
        if (previous != null && previous != PartOutcome.PENDING) return null // duplicate or already final
        if (previous == outcome) return null
        array.put(identity.part, outcome.name)
        val outcomes = (0 until identity.parts).map { outcomeAt(array, it) }
        val result = if (identity.kind == CallbackKind.SENT) SmsOutcomes.aggregateSent(outcomes) else SmsOutcomes.aggregateDelivered(outcomes)
        if (result != null) record.put(OWED_PREFIX + field, result.name)
        save(identity.commandId, record)
        result
    }

    /** Clears an owed aggregate after the core accepted it (or permanently refused it). */
    fun settled(commandId: String, result: NativeSendResult) = synchronized(LOCK) {
        val record = load(commandId) ?: return
        listOf(SENT, DELIVERED).forEach { field -> if (record.optString(OWED_PREFIX + field) == result.name) record.remove(OWED_PREFIX + field) }
        save(commandId, record)
    }

    fun owed(): List<Pair<String, NativeSendResult>> = synchronized(LOCK) {
        prefs.all.keys.filter { it.startsWith(PREFIX) }.flatMap { name ->
            val record = load(name.removePrefix(PREFIX)) ?: return@flatMap emptyList()
            listOf(SENT, DELIVERED).mapNotNull { field ->
                record.optString(OWED_PREFIX + field).takeIf { it.isNotEmpty() }
                    ?.let { owed -> NativeSendResult.entries.firstOrNull { it.name == owed } }
                    ?.let { name.removePrefix(PREFIX) to it }
            }
        }
    }

    /** Drops aggregation records whose delivery reports never arrived. */
    fun purgeOlderThan(maxAgeMillis: Long) = synchronized(LOCK) {
        val cutoff = now() - maxAgeMillis
        val editor = prefs.edit()
        prefs.all.forEach { (name, value) ->
            val created = (value as? String)?.let {
                try { JSONObject(it).optLong("created", 0L) } catch (_: JSONException) { 0L }
            }
            if (name.startsWith(PREFIX) && (created == null || created < cutoff)) editor.remove(name)
        }
        editor.commit()
    }

    /** Removes a record once nothing is owed and no further callback can change a claim. */
    private fun save(commandId: String, record: JSONObject) {
        val parts = record.optInt("parts")
        val owed = record.has(OWED_PREFIX + SENT) || record.has(OWED_PREFIX + DELIVERED)
        val sent = (0 until parts).map { outcomeAt(record.getJSONArray(SENT), it) }
        val delivered = (0 until parts).map { outcomeAt(record.getJSONArray(DELIVERED), it) }
        val deliveryFinal = delivered.none { it == null || it == PartOutcome.PENDING }
        val sentFailed = SmsOutcomes.aggregateSent(sent) == NativeSendResult.FAILED_CONFIRMED
        val editor = prefs.edit()
        if (!owed && (deliveryFinal || sentFailed)) editor.remove(key(commandId)) else editor.putString(key(commandId), record.toString())
        check(editor.commit())
    }

    private fun outcomeAt(array: JSONArray, index: Int): PartOutcome? =
        if (array.isNull(index)) null else PartOutcome.entries.firstOrNull { it.name == array.optString(index) }

    private fun load(commandId: String): JSONObject? = try {
        prefs.getString(key(commandId), null)?.let(::JSONObject)
    } catch (_: JSONException) {
        null
    }

    private fun key(commandId: String) = "$PREFIX$commandId"

    private companion object {
        const val PREFIX = "command:"
        const val SENT = "sent"
        const val DELIVERED = "delivered"
        const val OWED_PREFIX = "owed-"
        val LOCK = Any()
    }
}

/** Applies owed callback aggregates to the core exactly once from the host's point of view. */
object SmsCallbackReconciler {
    /** Returns true if the core accepted (or already had) the state. */
    fun apply(client: NativeClientInterface, store: SmsCallbackStore, commandId: String, result: NativeSendResult): Boolean {
        return try {
            client.recordSendResult(commandId, result)
            store.settled(commandId, result)
            true
        } catch (error: MobileBindingsException) {
            // Closed/Database are transient: keep owing. Any other refusal (e.g. an illegal
            // regression after a later state) is permanent: the core state stands.
            if (error !is MobileBindingsException.Closed && error !is MobileBindingsException.Database) {
                store.settled(commandId, result)
            }
            false
        }
    }

    fun replay(client: NativeClientInterface, store: SmsCallbackStore): Int =
        store.owed().count { (commandId, result) -> apply(client, store, commandId, result) }
}
