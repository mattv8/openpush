package dev.peppy.mobile

import android.Manifest
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.provider.Telephony
import android.telephony.SmsMessage
import androidx.annotation.VisibleForTesting
import androidx.core.content.ContextCompat
import uniffi.peppy_mobile_bindings.MobileBindingsException
import uniffi.peppy_mobile_bindings.NativeIncomingSms
import uniffi.peppy_mobile_bindings.NativeSendResult
import java.security.MessageDigest
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.Executor
import java.util.concurrent.ThreadPoolExecutor
import java.util.concurrent.TimeUnit

/** Seam for the one scheduling side effect, so tests can observe capture-before-schedule. */
object GatewayScheduler {
    @VisibleForTesting
    @Volatile
    internal var schedule: (Context) -> Unit = { GatewayWork.enqueue(it) }
}

/**
 * Receivers hand durable core work to this single owned thread via goAsync and finish only after
 * the commit and scheduling. The queue is bounded; when full, work runs on the caller instead of
 * being dropped. No network or key derivation ever runs here.
 */
object ReceiverWork {
    private val owned = ThreadPoolExecutor(1, 1, 30, TimeUnit.SECONDS, ArrayBlockingQueue(64), { runnable ->
        Thread(runnable, "peppy-receiver").apply { isDaemon = true }
    }, ThreadPoolExecutor.CallerRunsPolicy()).apply { allowCoreThreadTimeOut(true) }

    @VisibleForTesting
    @Volatile
    internal var executor: Executor = owned

    fun run(finish: () -> Unit, work: () -> Unit) {
        executor.execute {
            try {
                work()
            } finally {
                finish()
            }
        }
    }
}

/** One PDU's text as decoded by the platform. */
data class IncomingSmsPart(val address: String, val body: String, val timestampMillis: Long)

/**
 * Receives the system SMS_RECEIVED broadcast (sender must hold BROADCAST_SMS, enforced by the
 * manifest). It performs only local durable capture through the core, then schedules bounded
 * sync. No network, no Argon2, no logging of addresses or content.
 */
class IncomingSmsReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Telephony.Sms.Intents.SMS_RECEIVED_ACTION) return
        val messages: Array<SmsMessage?> = try {
            Telephony.Sms.Intents.getMessagesFromIntent(intent) ?: return
        } catch (_: RuntimeException) {
            return
        }
        val parts = messages.filterNotNull().mapNotNull { message ->
            message.originatingAddress?.let { IncomingSmsPart(it, message.messageBody.orEmpty(), message.timestampMillis) }
        }
        if (parts.isEmpty()) return
        val app = context.applicationContext
        val pending = goAsync()
        ReceiverWork.run(finish = { pending.finish() }) { IncomingCapture.handle(app, parts) }
    }
}

object IncomingCapture {
    /**
     * Captures every sender's concatenated parts, then schedules sync only if something was
     * durably captured. Returns the number of messages captured (including core duplicates).
     */
    fun handle(context: Context, parts: List<IncomingSmsPart>): Int {
        if (parts.isEmpty() ||
            !GatewayPolicyHost(context).permitsSmsCapture() ||
            ContextCompat.checkSelfPermission(context, Manifest.permission.RECEIVE_SMS) != PackageManager.PERMISSION_GRANTED
        ) {
            return 0
        }
        // The database opens with the Keystore-wrapped key even while the shared vault is locked;
        // the core stores the capture unsealed and seals it after unlock/key-cache import.
        val client = NativeGateway.open(context) ?: return 0
        var captured = 0
        for ((address, group) in parts.groupBy { it.address }) {
            val body = group.joinToString(separator = "") { it.body }
            val timestamp = group.first().timestampMillis
            try {
                client.captureIncoming(NativeIncomingSms(null, address, body, broadcastId(address, timestamp, body), false))
                captured++
            } catch (_: MobileBindingsException) {
                // Rejected by core validation; nothing durable to sync for this sender.
            }
        }
        if (captured > 0) GatewayScheduler.schedule(context)
        return captured
    }

    /**
     * Deterministic ID so a redelivered broadcast is a core duplicate rather than a second message.
     * Derived from the SMSC timestamp, sender and text; it reveals none of them.
     */
    internal fun broadcastId(address: String, timestampMillis: Long, body: String): String {
        val digest = MessageDigest.getInstance("SHA-256")
            .digest("$address\u0000$timestampMillis\u0000$body".toByteArray(Charsets.UTF_8))
        return "broadcast:" + digest.joinToString("") { "%02x".format(it) }
    }
}

/** Non-exported receiver reached only through this app's explicit callback PendingIntents. */
class SmsStatusReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val code = resultCode
        val app = context.applicationContext
        val pending = goAsync()
        ReceiverWork.run(finish = { pending.finish() }) { handle(app, intent, code) }
    }

    companion object {
        const val ACTION = "dev.peppy.mobile.SMS_CALLBACK"

        /**
         * Returns the aggregate result accepted by the core, or null when no claim was made (yet).
         * The aggregate is persisted as owed before the core write, so if the database is
         * unavailable or the process dies, the next sync pass replays it.
         */
        internal fun handle(context: Context, intent: Intent, resultCode: Int): NativeSendResult? {
            if (intent.action != ACTION) return null
            val identity = CallbackIdentity.parse(intent.data) ?: return null
            val outcome = when (identity.kind) {
                CallbackKind.SENT -> SmsOutcomes.sentPart(resultCode)
                CallbackKind.DELIVERED -> deliveryOutcome(intent)
            }
            val store = SmsCallbackStore(context)
            val result = store.record(identity, outcome) ?: return null
            val client = NativeGateway.open(context)
            val accepted = client != null && SmsCallbackReconciler.apply(client, store, identity.commandId, result)
            GatewayScheduler.schedule(context)
            return result.takeIf { accepted }
        }

        private fun deliveryOutcome(intent: Intent): PartOutcome {
            val format = intent.getStringExtra("format")
            val pdu = intent.getByteArrayExtra("pdu") ?: return PartOutcome.UNKNOWN
            val status = try {
                SmsMessage.createFromPdu(pdu, format)?.status
            } catch (_: RuntimeException) {
                null
            }
            return SmsOutcomes.deliveredPart(format, status)
        }
    }
}
