package dev.openpush.mobile

import android.app.PendingIntent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import uniffi.openpush_mobile_bindings.NativeClient
import uniffi.openpush_mobile_bindings.NativePermitState

/** Records carrier calls; can be told to throw like SmsManager does. */
class FakeCarrier(private val partSize: Int = 10, var failure: RuntimeException? = null) : SmsCarrier {
    data class Call(val subscriptionId: Int, val destination: String, val parts: List<String>, val sent: List<PendingIntent>, val delivered: List<PendingIntent>)

    val calls = mutableListOf<Call>()

    override fun divide(subscriptionId: Int, body: String) = body.chunked(partSize)

    override fun send(subscriptionId: Int, destination: String, parts: List<String>, sent: List<PendingIntent>, delivered: List<PendingIntent>) {
        failure?.let { throw it }
        calls += Call(subscriptionId, destination, parts, sent, delivered)
    }
}

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class SmsDispatcherTest : GatewayTestBase() {
    private val routes = SimRoutes.current(defaultSmsSubscriptionId = 3)
    private val route = SimRoutes.routeId(3)

    private fun dispatcher(client: NativeClient, carrier: SmsCarrier, granted: Boolean = true, current: List<SimRoute> = routes) =
        SmsDispatcher(context, client, carrier, current, granted)

    private fun permitState(client: NativeClient, commandId: String) = client.beginSendAttempt(commandId)

    @Test
    fun missingSendPermissionMakesNoCarrierCallAndConsumesNoPermit() {
        val client = enrollAndUnlock()
        val command = queueCommand(client, route, "hello")
        val carrier = FakeCarrier()
        dispatcher(client, carrier, granted = false).dispatch()
        assertTrue(carrier.calls.isEmpty())
        assertEquals(NativePermitState.PERMIT, permitState(client, command).state)
    }

    @Test
    fun staleOrUnknownSimRouteMakesNoCarrierCallAndConsumesNoPermit() {
        val client = enrollAndUnlock()
        val stale = queueCommand(client, SimRoutes.routeId(2), "to a removed SIM")
        val foreign = queueCommand(client, "sim-1", "simulator-style route")
        val carrier = FakeCarrier()
        val summary = dispatcher(client, carrier).dispatch()
        assertTrue(carrier.calls.isEmpty())
        assertEquals(2, summary.waiting)
        assertEquals(NativePermitState.PERMIT, permitState(client, stale).state)
        assertEquals(NativePermitState.PERMIT, permitState(client, foreign).state)
        // No SIM at all (default SMS subscription invalid) also yields no carrier call.
        val noSim = queueCommand(client, route, "no SIM")
        dispatcher(client, carrier, current = SimRoutes.current(defaultSmsSubscriptionId = -1)).dispatch()
        assertTrue(carrier.calls.isEmpty())
        assertEquals(NativePermitState.PERMIT, permitState(client, noSim).state)
    }

    @Test
    fun mmsCommandIsNeverConvertedToSms() {
        val client = enrollAndUnlock()
        val desktop = SharedVault.desktop
        val image = java.io.File.createTempFile("openpush", ".png").apply { writeBytes(ByteArray(64) { it.toByte() }); deleteOnExit() }
        val attachment = desktop.prepareAttachment(image.path, "image/png", "photo.png")
        val draft = desktop.createComposeDraft(null)
        val routeJson = org.json.JSONObject().put("gateway_device_id", deviceId).put("subscription_id", route).toString()
        val saved = desktop.saveComposeDraft(
            draft.draftId, draft.revision,
            uniffi.openpush_mobile_bindings.NativeComposeDraftUpdate("caption", listOf("+15551234567"), listOf(attachment.attachmentId), routeJson),
        )
        val queued = desktop.sendComposeDraft(saved.draftId, saved.revision)
        desktop.markAttachmentUploaded(attachment.attachmentId, java.util.UUID.randomUUID().toString())
        val wire = desktop.pendingOutboxJson().single { org.json.JSONObject(it).getString("envelope_id") == queued.envelopeId }
        desktop.ackOutbox(queued.envelopeId)
        client.ingestRaw(wire.toByteArray(), "1")
        client.applyPending(50uL)

        val carrier = FakeCarrier()
        dispatcher(client, carrier).dispatch()
        assertTrue("an MMS command must never be sent as text-only SMS", carrier.calls.isEmpty())
        assertEquals(NativePermitState.MEDIA_UNAVAILABLE, permitState(client, queued.commandId).state)
    }


    @Test
    fun explicitTextOnlyMmsIsNotDowngradedToSmsAndKeepsItsPermit() {
        val client = enrollAndUnlock()
        val command = queueCommand(client, route, "group text", listOf("+15551234567", "+15557654321"))
        assertEquals("mms", client.pendingCommands().single { it.commandId == command }.message.transport)
        val carrier = FakeCarrier()

        dispatcher(client, carrier).dispatch()

        assertTrue(carrier.calls.isEmpty())
        assertEquals(NativePermitState.PERMIT, permitState(client, command).state)
    }

    @Test
    fun permanentlyInvalidShapeIsTerminalWithoutAnyOsCall() {
        val client = enrollAndUnlock()
        val tooLong = queueCommand(client, route, "x".repeat(2_600)) // 260 parts at 10 chars each
        val carrier = FakeCarrier(partSize = 10)
        val summary = dispatcher(client, carrier).dispatch()
        assertTrue(carrier.calls.isEmpty())
        assertEquals(1, summary.refusedBeforeCarrier)
        assertEquals("FailedBeforeSubmission", permitState(client, tooLong).existingSendState)
    }

    @Test
    fun moreThanTheBudgetSchedulesAnotherPass() {
        val client = enrollAndUnlock()
        repeat(SmsDispatcher.MAX_COMMANDS_PER_RUN + 1) { queueCommand(client, route, "bulk $it") }
        val carrier = FakeCarrier()
        val first = dispatcher(client, carrier).dispatch()
        assertEquals(SmsDispatcher.MAX_COMMANDS_PER_RUN, first.submitted)
        assertTrue(first.moreRemaining)
        val second = dispatcher(client, carrier).dispatch()
        assertEquals(1, second.submitted)
        assertFalse(second.moreRemaining)
        assertEquals(SmsDispatcher.MAX_COMMANDS_PER_RUN + 1, carrier.calls.size)
    }

    @Test
    fun permittedCommandIsSentExactlyOnceWithUniqueCallbackIntents() {
        val client = enrollAndUnlock()
        val command = queueCommand(client, route, "a multipart body that spans parts")
        val carrier = FakeCarrier(partSize = 10)
        assertEquals(1, dispatcher(client, carrier).dispatch().submitted)
        val call = carrier.calls.single()
        assertEquals(3, call.subscriptionId)
        assertEquals("+15551234567", call.destination)
        assertEquals(4, call.parts.size)

        val all = call.sent + call.delivered
        val intents = all.map { shadowOf(it).savedIntent }
        assertEquals("every callback has its own PendingIntent identity", all.size, all.toSet().size)
        for (i in intents.indices) for (j in intents.indices) if (i != j) assertFalse(intents[i].filterEquals(intents[j]))
        val identities = intents.map { CallbackIdentity.parse(it.data)!! }
        assertEquals(call.parts.indices.toList(), identities.filter { it.kind == CallbackKind.SENT }.map { it.part })
        assertTrue(identities.all { it.commandId == command && it.parts == 4 })
        assertTrue(all.all { shadowOf(it).isBroadcast })
        assertEquals(SmsStatusReceiver::class.java.name, intents.first().component?.className)

        assertEquals("SubmittedToOs", permitState(client, command).existingSendState)
        dispatcher(client, carrier).dispatch()
        assertEquals("no resend after the permit is consumed", 1, carrier.calls.size)
    }

    @Test
    fun synchronousPlatformRefusalIsFailedBeforeSubmissionButUnknownErrorsMakeNoClaim() {
        val client = enrollAndUnlock()
        val refused = queueCommand(client, route, "refused")
        dispatcher(client, FakeCarrier(failure = SecurityException("revoked"))).dispatch()
        assertEquals("FailedBeforeSubmission", permitState(client, refused).existingSendState)

        val unknown = queueCommand(client, route, "unknown")
        dispatcher(client, FakeCarrier(failure = IllegalStateException("radio"))).dispatch()
        assertEquals(NativePermitState.ALREADY_ATTEMPTED, permitState(client, unknown).state)
        assertEquals("AttemptRecorded", permitState(client, unknown).existingSendState)
    }
}
