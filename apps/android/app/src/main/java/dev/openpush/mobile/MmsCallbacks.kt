package dev.openpush.mobile

import android.app.Activity
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import org.json.JSONObject
import uniffi.openpush_mobile_bindings.MobileBindingsException
import uniffi.openpush_mobile_bindings.NativeClientInterface
import uniffi.openpush_mobile_bindings.NativeSendResult
import java.io.File
import java.nio.file.Files
import java.nio.file.LinkOption.NOFOLLOW_LINKS
import java.util.concurrent.TimeUnit

/** Durable, content-free evidence for one MMS platform callback. */
class MmsCallbackStore(context: Context, private val now: () -> Long = System::currentTimeMillis) {
    private val prefs = context.getSharedPreferences("openpush-mms-callbacks", Context.MODE_PRIVATE)
    private val pduDir = File(context.filesDir, "mms-pdu")
    fun register(commandId: String): Boolean = synchronized(LOCK) {
        if (!valid(commandId)) return@synchronized false
        if (prefs.contains(key(commandId))) return@synchronized true
        prefs.edit().putString(key(commandId), JSONObject().put("created", now()).put("activity", now()).put("state", "pending").toString()).commit()
    }
    fun record(commandId: String, result: NativeSendResult) = synchronized(LOCK) {
        val record = load(commandId) ?: return@synchronized false
        if (record.has("owed") || record.optString("state") == "settled") return@synchronized false
        record.put("outcome", result.name).put("owed", true).put("state", "owed").put("activity", now())
        prefs.edit().putString(key(commandId), record.toString()).commit()
    }
    /** Records platform activity even for ambiguous result codes. */
    fun unknown(commandId: String, code: Int): Boolean = synchronized(LOCK) {
        val record = load(commandId) ?: return@synchronized false
        if (record.optBoolean("owed") || record.optString("state") == "settled") return@synchronized false
        record.put("state", "unknown").put("lastCode", code).put("activity", now())
        prefs.edit().putString(key(commandId), record.toString()).commit()
    }
    fun settled(commandId: String) = synchronized(LOCK) {
        val record = load(commandId) ?: return@synchronized false
        record.remove("owed")
        record.put("state", "settled").put("activity", now())
        val ok = prefs.edit().putString(key(commandId), record.toString()).commit()
        if (ok) file(commandId).delete()
        ok
    }
    fun owed(): List<Pair<String, NativeSendResult>> = synchronized(LOCK) { prefs.all.mapNotNull { (key, value) ->
        val json = try { JSONObject(value as String) } catch (_: Exception) { return@mapNotNull null }
        if (!json.optBoolean("owed")) null else NativeSendResult.entries.firstOrNull { it.name == json.optString("outcome") }?.let { key.removePrefix(PREFIX) to it }
    } }
    private fun load(commandId: String) = prefs.getString(key(commandId), null)?.let { try { JSONObject(it) } catch (_: Exception) { null } }
    private fun key(commandId: String) = PREFIX + commandId
    fun contains(commandId: String) = prefs.contains(key(commandId))
    /** Removes settled files immediately and pending/unknown files only after the required retention. */
    fun cleanup() = synchronized(LOCK) {
        val cutoff = now() - RETAIN_MILLIS
        prefs.all.keys.filter { it.startsWith(PREFIX) }.forEach { name ->
            val id = name.removePrefix(PREFIX); val record = load(id) ?: return@forEach
            if (!valid(id) || record.optBoolean("owed")) return@forEach
            val state = record.optString("state")
            if (state == "settled" || (state in setOf("pending", "unknown") && maxOf(record.optLong("created"), record.optLong("activity")) < cutoff)) {
                file(id).delete(); prefs.edit().remove(name).commit()
            }
        }
        pduDir.listFiles()?.forEach { candidate ->
            if (!candidate.name.matches(TEMP) && !candidate.name.matches(PDU)) return@forEach
            if (candidate.lastModified() < cutoff && !prefs.contains(key(candidate.name.removeSuffix(".pdu")))) candidate.delete()
        }
    }
    fun hasBudget(projectedBytes: Long): Boolean = synchronized(LOCK) {
        if (!pduDir.isDirectory || projectedBytes !in 0..MAX_BYTES) return@synchronized false
        var used = 0L
        pduDir.listFiles()?.forEach { file ->
            val length = try {
                val attributes = Files.readAttributes(file.toPath(), java.nio.file.attribute.BasicFileAttributes::class.java, NOFOLLOW_LINKS)
                if (attributes.isRegularFile) attributes.size() else 0L
            } catch (_: Exception) { return@synchronized false }
            if (length > MAX_BYTES - used) return@synchronized false
            used += length
        }
        used <= MAX_BYTES - projectedBytes
    }
    fun deletePrepared(commandId: String) = synchronized(LOCK) {
        if (valid(commandId) && !prefs.contains(key(commandId))) file(commandId).delete() else false
    }
    private fun file(commandId: String) = File(pduDir, "$commandId.pdu")
    private fun valid(commandId: String) = UUID.matches(commandId)
    private companion object {
        const val PREFIX = "command:"; val LOCK = Any(); const val MAX_BYTES = 512L * 1024 * 1024
        val RETAIN_MILLIS = TimeUnit.DAYS.toMillis(7); val UUID = Regex("[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
        val PDU = Regex("${UUID.pattern}.pdu"); val TEMP = Regex("\\.mms-pdu-.*\\.tmp")
    }
}

object MmsCallbackReconciler {
    fun apply(client: NativeClientInterface, store: MmsCallbackStore, commandId: String, result: NativeSendResult): Boolean = try {
        client.recordSendResult(commandId, result)
        store.settled(commandId)
    } catch (error: MobileBindingsException) {
        // InvalidRequest/NotFound prove this evidence can never be applied (no attempt or an
        // illegal regression). Every operational failure retains the owed record for replay.
        if (error is MobileBindingsException.InvalidRequest || error is MobileBindingsException.NotFound) store.settled(commandId)
        false
    }
    fun replay(client: NativeClientInterface, store: MmsCallbackStore) = store.owed().count { (id, result) -> apply(client, store, id, result) }
}

object MmsCallbackIntents {
    private val COMMAND_ID = Regex("[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
    fun create(context: Context, commandId: String): PendingIntent {
        require(COMMAND_ID.matches(commandId))
        val intent = Intent(context, MmsStatusReceiver::class.java).setAction(MmsStatusReceiver.ACTION)
            .setData(Uri.parse("openpush-mms://callback/$commandId"))
        val mutable = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) PendingIntent.FLAG_MUTABLE else 0
        return PendingIntent.getBroadcast(context, 0, intent, PendingIntent.FLAG_ONE_SHOT or mutable)
    }
    fun commandId(uri: Uri?) = uri?.takeIf { it.scheme == "openpush-mms" && it.authority == "callback" && it.pathSegments.size == 1 }
        ?.pathSegments?.single()?.takeIf(COMMAND_ID::matches)
}

/** Declared non-exported by the integration owner. Callback evidence is committed before core I/O. */
class MmsStatusReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val code = resultCode; val pending = goAsync(); val app = context.applicationContext
        ReceiverWork.run(finish = { pending.finish() }) { handle(app, intent, code) }
    }
    companion object {
        const val ACTION = "dev.openpush.mobile.MMS_CALLBACK"
        internal fun handle(context: Context, intent: Intent, resultCode: Int): NativeSendResult? {
            if (intent.action != ACTION) return null
            val commandId = MmsCallbackIntents.commandId(intent.data) ?: return null
            // RESULT_OK proves handoff to the platform, not delivery. Other result codes are
            // intentionally unknown: MMS implementations do not document them uniformly.
            val store = MmsCallbackStore(context)
            if (resultCode != Activity.RESULT_OK) { store.unknown(commandId, resultCode); GatewayScheduler.schedule(context); return null }
            val result = NativeSendResult.SENT
            if (!store.record(commandId, result)) return null
            NativeGateway.open(context)?.let { MmsCallbackReconciler.apply(it, store, commandId, result) }
            GatewayScheduler.schedule(context)
            return result
        }
    }
}
