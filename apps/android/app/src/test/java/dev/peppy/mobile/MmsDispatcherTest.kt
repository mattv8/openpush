package dev.peppy.mobile

import android.app.PendingIntent
import android.net.Uri
import androidx.core.content.FileProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import uniffi.peppy_mobile_bindings.NativeClient
import uniffi.peppy_mobile_bindings.NativeComposeDraftUpdate
import uniffi.peppy_mobile_bindings.NativePermitState
import java.io.File
import java.util.UUID

private class FakeMmsCarrier(var failure: RuntimeException? = null) : MmsCarrier {
    data class Call(val subscriptionId: Int, val pdu: Uri, val sent: PendingIntent)
    val calls = mutableListOf<Call>()
    override fun send(subscriptionId: Int, pdu: Uri, sent: PendingIntent) {
        failure?.let { throw it }
        calls += Call(subscriptionId, pdu, sent)
    }
}

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class MmsDispatcherTest : GatewayTestBase() {

    @Before
    fun clearMmsCallbackAndPduState() {
        context.getSharedPreferences("peppy-mms-callbacks", android.content.Context.MODE_PRIVATE).edit().clear().commit()
        File(context.filesDir, "mms-pdu").deleteRecursively()
        // AndroidX caches a PathStrategy only by authority. Robolectric gives each test a new
        // filesDir while loading FileProvider outside that sandbox, so stale roots must be reset.
        val cacheField = FileProvider::class.java.getDeclaredField("sCache").apply { isAccessible = true }
        val cache = cacheField.get(null) as MutableMap<*, *>
        synchronized(cache) { cache.clear() }
    }


    private val routes = SimRoutes.current(defaultSmsSubscriptionId = 3)
    private val route = SimRoutes.routeId(3)
    private val generous = MmsLimits(300L * 1024, 20, true, MmsLimits.Source.FALLBACK)
    private var mmsCursor = 0

    private fun dispatcher(
        client: NativeClient,
        carrier: MmsCarrier,
        granted: Boolean = true,
        enabled: Boolean = true,
        current: List<SimRoute> = routes,
        cap: MmsLimits = generous,
    ) = MmsDispatcher(context, client, carrier, current, granted, enabled, { cap })

    private fun queueMms(
        gateway: NativeClient,
        body: String,
        recipients: List<String>,
        attachment: File? = null,
        installMedia: Boolean = true,
    ): Pair<String, String?> {
        val desktop = SharedVault.desktop
        val prepared = attachment?.let { desktop.prepareAttachment(it.path, "image/png", "photo.png") }
        val draft = desktop.createComposeDraft(null)
        val routeJson = org.json.JSONObject().put("gateway_device_id", deviceId).put("subscription_id", route).toString()
        val saved = desktop.saveComposeDraft(
            draft.draftId,
            draft.revision,
            NativeComposeDraftUpdate(body, recipients, listOfNotNull(prepared?.attachmentId), routeJson),
        )
        val queued = desktop.sendComposeDraft(saved.draftId, saved.revision)
        prepared?.let { desktop.markAttachmentUploaded(it.attachmentId, UUID.randomUUID().toString()) }
        val wire = desktop.pendingOutboxJson().single { org.json.JSONObject(it).getString("envelope_id") == queued.envelopeId }
        desktop.ackOutbox(queued.envelopeId)
        gateway.ingestRaw(wire.toByteArray(), (++mmsCursor).toString())
        gateway.applyPending(50uL)
        if (prepared != null && installMedia) {
            val downloaded = File.createTempFile("peppy-download", ".ppss").apply {
                writeBytes(File(desktop.nativeCipherFileForUpload(prepared.attachmentId)).readBytes())
                deleteOnExit()
            }
            gateway.installDownloadedAttachment(prepared.attachmentId, downloaded.path)
        }
        return queued.commandId to prepared?.attachmentId
    }

    @Test
    fun textOnlyGroupCreatesOnePduAndSubmitsExactlyOnce() {
        val client = enrollAndUnlock()
        val (command, _) = queueMms(client, "group hello", listOf("+15551234567", "+15557654321"))
        assertEquals("mms", client.pendingCommands().single { it.commandId == command }.message.transport)
        val carrier = FakeMmsCarrier()

        val summary = dispatcher(client, carrier).dispatch()
        assertEquals(summary.toString(), 1, summary.submitted)
        val pdu = File(context.filesDir, "mms-pdu/$command.pdu")
        assertTrue(pdu.isFile)
        assertTrue(pdu.readBytes().toString(Charsets.ISO_8859_1).contains("group hello"))
        assertEquals(3, carrier.calls.single().subscriptionId)
        assertEquals("SubmittedToOs", client.beginSendAttempt(command).existingSendState)

        dispatcher(client, carrier).dispatch()
        assertEquals("a consumed permit is never resent", 1, carrier.calls.size)
    }

    @Test
    fun mediaOnlyCreatesOnePduAndReleasesPlaintextHandle() {
        val client = enrollAndUnlock()
        val image = File.createTempFile("peppy-mms", ".png").apply { writeBytes(ByteArray(128) { it.toByte() }); deleteOnExit() }
        val (command, attachmentId) = queueMms(client, "", listOf("+15551234567"), image)
        val probe = client.openNativePlaintextFile(checkNotNull(attachmentId))
        val plaintextDirectory = File(probe.nativePlaintextPath()).parentFile!!
        probe.close()
        val carrier = FakeMmsCarrier()

        val summary = dispatcher(client, carrier).dispatch()
        assertEquals(summary.toString(), 1, summary.submitted)
        assertEquals(1, carrier.calls.size)
        assertTrue(File(context.filesDir, "mms-pdu/$command.pdu").length() > image.length())
        assertFalse("native plaintext handles are closed after compose", plaintextDirectory.listFiles()?.any() == true)
    }

    @Test
    fun disabledPreflightConsumesNoPermit() {
        val client = enrollAndUnlock()
        val (id, _) = queueMms(client, "disabled", listOf("+15551234567", "+15557654321"))
        dispatcher(client, FakeMmsCarrier(), enabled = false).dispatch()
        assertEquals(NativePermitState.PERMIT, client.beginSendAttempt(id).state)
    }

    @Test
    fun missingPermissionConsumesNoPermit() {
        val client = enrollAndUnlock()
        val (id, _) = queueMms(client, "permission", listOf("+15551234567", "+15557654321"))
        dispatcher(client, FakeMmsCarrier(), granted = false).dispatch()
        assertEquals(NativePermitState.PERMIT, client.beginSendAttempt(id).state)
    }

    @Test
    fun staleSimConsumesNoPermit() {
        val client = enrollAndUnlock()
        val (id, _) = queueMms(client, "sim", listOf("+15551234567", "+15557654321"))
        dispatcher(client, FakeMmsCarrier(), current = emptyList()).dispatch()
        assertEquals(NativePermitState.PERMIT, client.beginSendAttempt(id).state)
    }

    @Test
    fun missingMediaAndIoConsumeNoPermit() {
        val client = enrollAndUnlock()
        val image = File.createTempFile("missing", ".png").apply { writeBytes(byteArrayOf(1)); deleteOnExit() }
        val (missing, _) = queueMms(client, "", listOf("+15551234567"), image, installMedia = false)
        dispatcher(client, FakeMmsCarrier()).dispatch()
        assertEquals(NativePermitState.MEDIA_UNAVAILABLE, client.beginSendAttempt(missing).state)

        val (io, _) = queueMms(client, "io", listOf("+15551234567", "+15557654321"))
        val pduDirectory = File(context.filesDir, "mms-pdu").apply { deleteRecursively(); writeText("not a directory") }
        try {
            dispatcher(client, FakeMmsCarrier()).dispatch()
            assertEquals(NativePermitState.PERMIT, client.beginSendAttempt(io).state)
        } finally {
            pduDirectory.delete()
            pduDirectory.mkdirs()
        }
    }

    @Test
    fun sizeFailureMakesNoCarrierCallAndClosesPlaintext() {
        val client = enrollAndUnlock()
        val image = File.createTempFile("oversized", ".png").apply { writeBytes(ByteArray(128)); deleteOnExit() }
        val (tooLarge, attachmentId) = queueMms(client, "", listOf("+15551234567"), image)
        val probe = client.openNativePlaintextFile(checkNotNull(attachmentId))
        val plaintextDirectory = File(probe.nativePlaintextPath()).parentFile!!
        probe.close()
        val carrier = FakeMmsCarrier()
        dispatcher(client, carrier, cap = generous.copy(maximumPduBytes = 10)).dispatch()
        assertTrue(carrier.calls.isEmpty())
        assertEquals("FailedBeforeSubmission", client.beginSendAttempt(tooLarge).existingSendState)
        assertFalse("a refused compose closes every plaintext handle", plaintextDirectory.listFiles()?.any() == true)

    }

    @Test
    fun unknownCarrierFailureDoesNotResend() {
        val client = enrollAndUnlock()
        val (unknown, _) = queueMms(client, "unknown", listOf("+15551234567", "+15557654321"))
        val summary = dispatcher(client, FakeMmsCarrier(IllegalStateException("radio"))).dispatch()
        assertEquals(summary.toString(), NativePermitState.ALREADY_ATTEMPTED, client.beginSendAttempt(unknown).state)
        assertEquals("AttemptRecorded", client.beginSendAttempt(unknown).existingSendState)
        val restartedCarrier = FakeMmsCarrier()
        dispatcher(client, restartedCarrier).dispatch()
        assertTrue(restartedCarrier.calls.isEmpty())
    }

}
