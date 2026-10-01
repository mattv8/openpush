package dev.openpush.mobile

import android.app.PendingIntent
import android.content.Context
import android.os.Build
import android.telephony.SmsManager
import uniffi.openpush_mobile_bindings.MobileBindingsException
import uniffi.openpush_mobile_bindings.NativeClientInterface
import uniffi.openpush_mobile_bindings.NativePermitState
import uniffi.openpush_mobile_bindings.NativeSendResult

/** The only carrier boundary. Production uses [AndroidSmsCarrier]; tests record calls. */
interface SmsCarrier {
    fun divide(subscriptionId: Int, body: String): List<String>
    fun send(subscriptionId: Int, destination: String, parts: List<String>, sent: List<PendingIntent>, delivered: List<PendingIntent>)
}

class AndroidSmsCarrier(private val context: Context) : SmsCarrier {
    private fun manager(subscriptionId: Int): SmsManager =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            context.getSystemService(SmsManager::class.java).createForSubscriptionId(subscriptionId)
        } else {
            @Suppress("DEPRECATION")
            SmsManager.getSmsManagerForSubscriptionId(subscriptionId)
        }

    override fun divide(subscriptionId: Int, body: String): List<String> = manager(subscriptionId).divideMessage(body)

    override fun send(subscriptionId: Int, destination: String, parts: List<String>, sent: List<PendingIntent>, delivered: List<PendingIntent>) {
        val manager = manager(subscriptionId)
        if (parts.size == 1) {
            manager.sendTextMessage(destination, null, parts[0], sent[0], delivered[0])
        } else {
            manager.sendMultipartTextMessage(destination, null, ArrayList(parts), ArrayList(sent), ArrayList(delivered))
        }
    }
}

data class DispatchSummary(
    val submitted: Int,
    /** Commands terminally recorded FAILED_BEFORE_SUBMISSION without any OS call. */
    val refusedBeforeCarrier: Int,
    /** Commands left pending: missing permission, no matching live route, or media blocked. */
    val waiting: Int,
    /** The per-run budget stopped the loop with commands still unexamined: schedule another pass. */
    val moreRemaining: Boolean,
)

/**
 * Executes pending carrier commands. Per command, strictly in order:
 *  1. permanently invalid shapes for this SMS adapter (not exactly one recipient, empty text, any
 *     attachment, more than 255 parts) take the core permit only to record FAILED_BEFORE_SUBMISSION;
 *     the OS is never called. MMS stays pending if the core blocks its permit (media unavailable);
 *  2. transient host preconditions (SEND_SMS granted, exact live route) leave the command pending
 *     with no permit;
 *  3. the core permit (`beginSendAttempt` == PERMIT) durably records the attempt;
 *  4. callback aggregation is registered with unique PendingIntents;
 *  5. exactly one carrier call; no resend is ever attempted.
 */
class SmsDispatcher(
    private val context: Context,
    private val client: NativeClientInterface,
    private val carrier: SmsCarrier,
    private val routes: List<SimRoute>,
    private val sendPermissionGranted: Boolean,
    private val callbacks: SmsCallbackStore = SmsCallbackStore(context),
) {
    fun dispatch(limit: Int = MAX_COMMANDS_PER_RUN): DispatchSummary {
        var submitted = 0
        var refused = 0
        var waiting = 0
        var permits = 0
        var more = false
        for (pending in client.pendingCommands()) {
            // Bound permit/carrier work per run; waiting commands do not starve later ones.
            if (permits >= limit) {
                more = true
                break
            }
            val message = pending.message
            val attachments = try {
                client.pendingCommandAttachmentCount(pending.commandId)
            } catch (_: MobileBindingsException.NotFound) {
                continue // no longer pending
            }
            if (message.recipients.size != 1 || message.body.isEmpty() || attachments > 0uL) {
                if (refuseBeforeCarrier(pending.commandId)) {
                    refused++
                    permits++
                } else {
                    waiting++
                }
                continue
            }
            val route = SimRoutes.resolve(pending.subscriptionId, routes)
            if (!sendPermissionGranted || route == null) {
                waiting++
                continue
            }
            val parts = try {
                carrier.divide(route.subscriptionId, message.body)
            } catch (_: RuntimeException) {
                waiting++
                continue
            }
            if (parts.isEmpty() || parts.size > CallbackIdentity.MAX_PARTS) {
                if (refuseBeforeCarrier(pending.commandId)) {
                    refused++
                    permits++
                } else {
                    waiting++
                }
                continue
            }
            val permit = client.beginSendAttempt(pending.commandId)
            val command = permit.command
            if (permit.state != NativePermitState.PERMIT || command == null || command.commandId != pending.commandId) {
                waiting++
                continue
            }
            permits++
            val recipient = command.message.recipients.single()
            val sent = parts.indices.map { SmsCallbackIntents.create(context, CallbackIdentity(CallbackKind.SENT, command.commandId, it, parts.size)) }
            val delivered = parts.indices.map { SmsCallbackIntents.create(context, CallbackIdentity(CallbackKind.DELIVERED, command.commandId, it, parts.size)) }
            callbacks.register(command.commandId, parts.size)
            try {
                carrier.send(route.subscriptionId, recipient, parts, sent, delivered)
            } catch (_: SecurityException) {
                record(command.commandId, NativeSendResult.FAILED_BEFORE_SUBMISSION)
                refused++
                continue
            } catch (_: IllegalArgumentException) {
                record(command.commandId, NativeSendResult.FAILED_BEFORE_SUBMISSION)
                refused++
                continue
            } catch (_: RuntimeException) {
                // Unknown platform failure: the core keeps AttemptRecorded (outcome unknown).
                continue
            }
            record(command.commandId, NativeSendResult.SUBMITTED)
            submitted++
        }
        return DispatchSummary(submitted, refused, waiting, more)
    }

    /** Takes the permit only to record a truthful terminal refusal; never calls the OS. */
    private fun refuseBeforeCarrier(commandId: String): Boolean {
        val permit = client.beginSendAttempt(commandId)
        if (permit.state != NativePermitState.PERMIT || permit.command?.commandId != commandId) return false
        record(commandId, NativeSendResult.FAILED_BEFORE_SUBMISSION)
        return true
    }

    /** A racing SENT callback may already have advanced the state; the core rejects regressions. */
    private fun record(commandId: String, result: NativeSendResult) {
        try {
            client.recordSendResult(commandId, result)
        } catch (_: MobileBindingsException) {
        }
    }

    companion object {
        const val MAX_COMMANDS_PER_RUN = 10
    }
}
