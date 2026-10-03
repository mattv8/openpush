package dev.peppy.mobile

import android.app.PendingIntent
import android.content.Context
import android.net.Uri
import androidx.core.content.FileProvider
import dev.peppy.mobile.mms.pdu.MmsFilePart
import dev.peppy.mobile.mms.pdu.MmsPduComposer
import dev.peppy.mobile.mms.pdu.MmsPduException
import dev.peppy.mobile.mms.pdu.MmsPduRequest
import dev.peppy.mobile.mms.pdu.MmsPduValidationException
import dev.peppy.mobile.mms.pdu.MmsRecipient
import dev.peppy.mobile.mms.pdu.MmsRecipientKind
import uniffi.peppy_mobile_bindings.MobileBindingsException
import uniffi.peppy_mobile_bindings.NativeClientInterface
import uniffi.peppy_mobile_bindings.NativePermitState
import uniffi.peppy_mobile_bindings.NativeSendResult
import java.io.File

interface MmsCarrier { fun send(subscriptionId: Int, pdu: Uri, sent: PendingIntent) }

class AndroidMmsCarrier(private val context: Context) : MmsCarrier {
    override fun send(subscriptionId: Int, pdu: Uri, sent: PendingIntent) {
        val manager = if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.S)
            context.getSystemService(android.telephony.SmsManager::class.java).createForSubscriptionId(subscriptionId)
        else @Suppress("DEPRECATION") android.telephony.SmsManager.getSmsManagerForSubscriptionId(subscriptionId)
        manager.sendMultimediaMessage(context, pdu, null, null, sent)
    }
}

/** Explicit-MMS carrier dispatch; all preflight work occurs before a durable permit. */
class MmsDispatcher(
    private val context: Context, private val client: NativeClientInterface, private val carrier: MmsCarrier,
    private val routes: List<SimRoute>, private val sendPermissionGranted: Boolean, private val enabled: Boolean,
    private val limits: (Int) -> MmsLimits = { MmsLimits.forSubscription(context, it) },
    private val callbacks: MmsCallbackStore = MmsCallbackStore(context),
) {
    fun dispatch(limit: Int = SmsDispatcher.MAX_COMMANDS_PER_RUN): DispatchSummary = synchronized(DISPATCH_LOCK) {
        var submitted = 0; var refused = 0; var waiting = 0; var permits = 0; var more = false
        for (pending in client.pendingCommands()) {
            if (pending.message.transport != "mms") continue
            if (permits >= limit) { more = true; break }
            val route = SimRoutes.resolve(pending.subscriptionId, routes)
            if (!enabled || !sendPermissionGranted || route == null) { waiting++; continue }
            val cap = limits(route.subscriptionId)
            if (!cap.enabled || pending.message.recipients.size !in 1..cap.maximumRecipients ||
                (pending.message.body.isEmpty() && pending.message.attachmentIds.isEmpty())) {
                if (refuse(pending.commandId)) { refused++; permits++ } else waiting++
                continue
            }
            val directory = File(context.filesDir, "mms-pdu")
            if ((!directory.exists() && !directory.mkdirs()) || !directory.isDirectory || callbacks.contains(pending.commandId) || !callbacks.hasBudget(cap.maximumPduBytes)) {
                waiting++; continue
            }
            val output = File(directory, "${pending.commandId}.pdu")
            val prepared = try { compose(pending.commandId, pending.message, cap, output) }
            catch (error: MmsPduValidationException) {
                callbacks.deletePrepared(pending.commandId)
                if (error.message == "output directory does not exist" || error.message == "part is not a regular file") {
                    waiting++
                } else if (refuse(pending.commandId)) {
                    refused++; permits++
                } else waiting++
                continue
            } catch (_: MmsPduException) { null } catch (_: MobileBindingsException) { null }
            if (prepared == null) { waiting++; continue } // transient media/IO failure: no permit
            val sent = try { MmsCallbackIntents.create(context, pending.commandId) } catch (_: RuntimeException) { callbacks.deletePrepared(pending.commandId); waiting++; continue }
            val uri = try { FileProvider.getUriForFile(context, "${context.packageName}.mms-files", output) }
            catch (_: IllegalArgumentException) { callbacks.deletePrepared(pending.commandId); waiting++; continue }
            val permit = client.beginSendAttempt(pending.commandId)
            val command = permit.command
            if (permit.state != NativePermitState.PERMIT || command?.commandId != pending.commandId) { callbacks.deletePrepared(pending.commandId); waiting++; continue }
            permits++
            if (!callbacks.register(command.commandId)) { record(command.commandId, NativeSendResult.FAILED_BEFORE_SUBMISSION); callbacks.deletePrepared(command.commandId); refused++; continue }
            try { carrier.send(route.subscriptionId, uri, sent) }
            catch (_: SecurityException) { failBeforeSubmission(command.commandId); refused++; continue }
            catch (_: IllegalArgumentException) { failBeforeSubmission(command.commandId); refused++; continue }
            catch (_: RuntimeException) { continue }
            record(command.commandId, NativeSendResult.SUBMITTED); submitted++
        }
        return DispatchSummary(submitted, refused, waiting, more)
    }
    private fun compose(commandId: String, message: uniffi.peppy_mobile_bindings.NativeMessage, cap: MmsLimits, output: File): File {
        val handles = mutableListOf<uniffi.peppy_mobile_bindings.NativePlaintextHandle>()
        try {
            message.attachmentIds.forEach { handles += client.openNativePlaintextFile(it) }
            val parts = handles.zip(message.attachmentIds).map { (handle, id) ->
                val info = client.attachmentInfo(id)
                MmsFilePart(File(handle.nativePlaintextPath()), info.mediaType, info.displayName, null)
            }
            MmsPduComposer.compose(MmsPduRequest("peppy-$commandId", message.recipients.map { MmsRecipient(it, MmsRecipientKind.TO) }, message.subject, message.body, parts, cap.maximumPduBytes, output))
            return output
        } finally { handles.forEach { it.close() } }
    }
    private fun refuse(commandId: String): Boolean {
        val permit = client.beginSendAttempt(commandId)
        if (permit.state != NativePermitState.PERMIT || permit.command?.commandId != commandId) return false
        record(commandId, NativeSendResult.FAILED_BEFORE_SUBMISSION); return true
    }
    private fun failBeforeSubmission(id: String) {
        if (callbacks.record(id, NativeSendResult.FAILED_BEFORE_SUBMISSION)) {
            MmsCallbackReconciler.apply(client, callbacks, id, NativeSendResult.FAILED_BEFORE_SUBMISSION)
        }
    }
    private fun record(id: String, result: NativeSendResult) { try { client.recordSendResult(id, result) } catch (_: MobileBindingsException) {} }
    private companion object { val DISPATCH_LOCK = Any() }
}
