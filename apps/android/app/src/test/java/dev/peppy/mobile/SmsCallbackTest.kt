package dev.peppy.mobile

import android.app.Activity
import android.content.Intent
import android.telephony.SmsManager
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import uniffi.peppy_mobile_bindings.NativeSendResult
import java.util.UUID

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class SmsCallbackTest : GatewayTestBase() {
    private val ok = PartOutcome.OK
    private val failed = PartOutcome.FAILED
    private val unknown = PartOutcome.UNKNOWN

    @Test
    fun outcomeMappingIsConservative() {
        assertEquals(ok, SmsOutcomes.sentPart(Activity.RESULT_OK))
        assertEquals(failed, SmsOutcomes.sentPart(SmsManager.RESULT_ERROR_NO_SERVICE))
        assertEquals("generic failure may be a lost ack", unknown, SmsOutcomes.sentPart(SmsManager.RESULT_ERROR_GENERIC_FAILURE))
        assertNull(SmsOutcomes.aggregateSent(listOf(SmsOutcomes.sentPart(SmsManager.RESULT_ERROR_GENERIC_FAILURE))))
        assertEquals(unknown, SmsOutcomes.sentPart(Activity.RESULT_CANCELED))
        assertEquals(unknown, SmsOutcomes.sentPart(9_999))
        assertEquals(NativeSendResult.SENT, SmsOutcomes.aggregateSent(listOf(ok, ok)))
        assertEquals(NativeSendResult.FAILED_CONFIRMED, SmsOutcomes.aggregateSent(listOf(failed, failed)))
        assertNull("partial transmission is never claimed", SmsOutcomes.aggregateSent(listOf(ok, failed)))
        assertNull(SmsOutcomes.aggregateSent(listOf(ok, unknown)))
        assertNull("waits for every part", SmsOutcomes.aggregateSent(listOf(ok, null)))
        assertEquals(ok, SmsOutcomes.deliveredPart("3gpp", 0x00))
        assertEquals(PartOutcome.PENDING, SmsOutcomes.deliveredPart("3gpp", 0x20))
        assertEquals(failed, SmsOutcomes.deliveredPart("3gpp", 0x40))
        assertEquals(unknown, SmsOutcomes.deliveredPart("3gpp", 0x80))
        assertEquals(unknown, SmsOutcomes.deliveredPart("3gpp2", 0x00))
        assertEquals(unknown, SmsOutcomes.deliveredPart("3gpp", null))
    }

    @Test
    fun callbackStoreAggregatesOnceAndIgnoresDuplicatesAndForgeries() {
        val store = SmsCallbackStore(context)
        val command = UUID.randomUUID().toString()
        store.register(command, 2)
        val part0 = CallbackIdentity(CallbackKind.SENT, command, 0, 2)
        assertNull(store.record(part0, ok))
        assertNull("duplicate part callback is not counted twice", store.record(part0, ok))
        assertNull("part-count mismatch is ignored", store.record(CallbackIdentity(CallbackKind.SENT, command, 1, 3), ok))
        assertEquals(NativeSendResult.SENT, store.record(CallbackIdentity(CallbackKind.SENT, command, 1, 2), ok))
        assertNull("unregistered command", store.record(CallbackIdentity(CallbackKind.SENT, UUID.randomUUID().toString(), 0, 1), ok))
        assertNull(store.record(CallbackIdentity(CallbackKind.DELIVERED, command, 0, 2), ok))
        assertEquals(NativeSendResult.DELIVERED, store.record(CallbackIdentity(CallbackKind.DELIVERED, command, 1, 2), ok))
    }

    @Test
    fun callbackIdentityRejectsMalformedUris() {
        val command = UUID.randomUUID().toString()
        val good = CallbackIdentity(CallbackKind.DELIVERED, command, 1, 2)
        assertEquals(good, CallbackIdentity.parse(good.uri()))
        listOf(
            "peppy-sms://callback/sent/$command/2/2",
            "peppy-sms://callback/sent/not-a-command/0/1",
            "peppy-sms://other/sent/$command/0/1",
            "https://callback/sent/$command/0/1",
            "peppy-sms://callback/read/$command/0/1",
            "peppy-sms://callback/sent/$command/00/1",
            "peppy-sms://callback/sent/$command/+0/1",
            "peppy-sms://callback/sent/$command/0/1?x=1",
        ).forEach { assertNull(it, CallbackIdentity.parse(android.net.Uri.parse(it))) }
    }

    @Test
    fun statusReceiverRecordsAggregateInCoreAfterColdStart() {
        val client = enrollAndUnlock()
        val command = queueCommand(client, SimRoutes.routeId(3), "two parts here")
        val carrier = FakeCarrier(partSize = 8)
        SmsDispatcher(context, client, carrier, SimRoutes.current(3), sendPermissionGranted = true).dispatch()
        val sent = carrier.calls.single().sent.map { shadowOf(it).savedIntent }
        NativeGateway.closeForTest() // callbacks commonly arrive in a fresh process

        assertNull(SmsStatusReceiver.handle(context, sent[0], Activity.RESULT_OK))
        // On reopen the core conservatively reclassifies the in-flight submission as unknown.
        assertEquals("OutcomeUnknown", checkNotNull(NativeGateway.open(context)).beginSendAttempt(command).existingSendState)
        assertEquals(NativeSendResult.SENT, SmsStatusReceiver.handle(context, sent[1], Activity.RESULT_OK))
        assertEquals("Sent", checkNotNull(NativeGateway.open(context)).beginSendAttempt(command).existingSendState)
        assertEquals(listOf("sync"), scheduled)

        // Forged/unknown intents never touch the core.
        assertNull(SmsStatusReceiver.handle(context, Intent(SmsStatusReceiver.ACTION), Activity.RESULT_OK))
        assertNull(SmsStatusReceiver.handle(context, Intent("other").setData(sent[0].data), Activity.RESULT_OK))
    }

    @Test
    fun unknownResultCodeLeavesCommandSubmitted() {
        val client = enrollAndUnlock()
        val command = queueCommand(client, SimRoutes.routeId(3), "short")
        val carrier = FakeCarrier()
        SmsDispatcher(context, client, carrier, SimRoutes.current(3), sendPermissionGranted = true).dispatch()
        val sent = shadowOf(carrier.calls.single().sent.single()).savedIntent
        assertNull(SmsStatusReceiver.handle(context, sent, 12_345))
        assertEquals("SubmittedToOs", client.beginSendAttempt(command).existingSendState)
        // A delivery report without a parsable PDU is not proof of delivery.
        val delivered = shadowOf(carrier.calls.single().delivered.single()).savedIntent
        assertNull(SmsStatusReceiver.handle(context, delivered, Activity.RESULT_OK))
        assertEquals("SubmittedToOs", client.beginSendAttempt(command).existingSendState)
    }

    @Test
    fun temporaryDeliveryReportIsSupersededByTheFinalOne() {
        val store = SmsCallbackStore(context)
        val command = UUID.randomUUID().toString()
        store.register(command, 1)
        val part = CallbackIdentity(CallbackKind.DELIVERED, command, 0, 1)
        assertNull(store.record(part, PartOutcome.PENDING))
        assertEquals(NativeSendResult.DELIVERED, store.record(part, ok))
        assertNull("final outcomes are immutable", store.record(part, failed))

        val other = UUID.randomUUID().toString()
        store.register(other, 1)
        val otherPart = CallbackIdentity(CallbackKind.DELIVERED, other, 0, 1)
        assertNull(store.record(otherPart, PartOutcome.PENDING))
        assertNull("permanent delivery failure never claims anything", store.record(otherPart, failed))
    }

    @Test
    fun deliveryIntentsMayFireRepeatedlyButSentIntentsOnce() {
        val client = enrollAndUnlock()
        queueCommand(client, SimRoutes.routeId(3), "flags")
        val carrier = FakeCarrier()
        SmsDispatcher(context, client, carrier, SimRoutes.current(3), sendPermissionGranted = true).dispatch()
        val call = carrier.calls.single()
        assertTrue(shadowOf(call.sent.single()).flags and android.app.PendingIntent.FLAG_ONE_SHOT != 0)
        assertEquals(0, shadowOf(call.delivered.single()).flags and android.app.PendingIntent.FLAG_ONE_SHOT)
    }

    @Test
    fun aggregateOwedToTheCoreIsReplayedAfterACrashOrUnavailableDatabase() {
        val client = enrollAndUnlock()
        val command = queueCommand(client, SimRoutes.routeId(3), "owed")
        val carrier = FakeCarrier()
        SmsDispatcher(context, client, carrier, SimRoutes.current(3), sendPermissionGranted = true).dispatch()
        val sent = shadowOf(carrier.calls.single().sent.single()).savedIntent
        NativeGateway.closeForTest()
        val key = keys.key
        keys.key = null // database cannot open: the callback must not be lost
        assertNull(SmsStatusReceiver.handle(context, sent, Activity.RESULT_OK))
        val store = SmsCallbackStore(context)
        assertEquals(listOf(command to NativeSendResult.SENT), store.owed())

        keys.key = key
        val reopened = checkNotNull(NativeGateway.open(context))
        assertEquals(1, SmsCallbackReconciler.replay(reopened, store))
        assertEquals("Sent", reopened.beginSendAttempt(command).existingSendState)
        assertTrue(store.owed().isEmpty())
        assertEquals("replay is idempotent", 0, SmsCallbackReconciler.replay(reopened, store))
    }

    @Test
    fun genericCarrierFailureLeavesTheOutcomeUnknown() {
        val client = enrollAndUnlock()
        val command = queueCommand(client, SimRoutes.routeId(3), "generic")
        val carrier = FakeCarrier()
        SmsDispatcher(context, client, carrier, SimRoutes.current(3), sendPermissionGranted = true).dispatch()
        val sent = shadowOf(carrier.calls.single().sent.single()).savedIntent
        assertNull(SmsStatusReceiver.handle(context, sent, SmsManager.RESULT_ERROR_GENERIC_FAILURE))
        assertEquals("SubmittedToOs", client.beginSendAttempt(command).existingSendState)
    }
}
