package dev.openpush.mobile

import android.app.Activity
import android.content.Intent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import uniffi.openpush_mobile_bindings.NativeSendResult
import java.io.File
import java.util.UUID
import java.util.concurrent.Executor
import java.util.concurrent.TimeUnit

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class MmsCallbacksTest : GatewayTestBase() {

    @Before
    fun clearMmsCallbackAndPduState() {
        context.getSharedPreferences("openpush-mms-callbacks", android.content.Context.MODE_PRIVATE).edit().clear().commit()
        File(context.filesDir, "mms-pdu").deleteRecursively()
    }


    private fun pdu(id: String) = File(context.filesDir, "mms-pdu/$id.pdu")
    private fun callback(id: String) = Intent(MmsStatusReceiver.ACTION).setData(android.net.Uri.parse("openpush-mms://callback/$id"))

    @Test
    fun pendingAndUnknownAttemptsAgeAfterSevenDaysButOwedEvidenceDoesNot() {
        var now = 1_000_000L
        val store = MmsCallbackStore(context) { now }
        val pending = UUID.randomUUID().toString()
        val unknown = UUID.randomUUID().toString()
        val owed = UUID.randomUUID().toString()
        listOf(pending, unknown, owed).forEach { id ->
            pdu(id).parentFile!!.mkdirs(); pdu(id).writeBytes(byteArrayOf(1)); assertTrue(store.register(id))
        }
        assertTrue(store.unknown(unknown, 1234))
        assertTrue(store.record(owed, NativeSendResult.SENT))

        now += TimeUnit.DAYS.toMillis(7) + 1
        store.cleanup()

        assertFalse("a missing callback cannot retain plaintext forever", pdu(pending).exists())
        assertFalse(pdu(unknown).exists())
        assertFalse(store.contains(pending))
        assertFalse(store.contains(unknown))
        assertTrue("unreplayed callback evidence is never expired", pdu(owed).exists())
        assertEquals(listOf(owed to NativeSendResult.SENT), store.owed())
    }

    @Test
    fun budgetCountsTemporaryRegularFilesAndIgnoresSymlinks() {
        val directory = File(context.filesDir, "mms-pdu").apply { mkdirs() }
        val store = MmsCallbackStore(context)
        val temporary = File(directory, ".mms-pdu-budget.tmp")
        java.io.RandomAccessFile(temporary, "rw").use { it.setLength(512L * 1024 * 1024) }
        assertFalse("temporary compose output must count toward the hard ceiling", store.hasBudget(1))

        temporary.delete()
        val external = File.createTempFile("outside-mms", ".bin").apply { writeBytes(ByteArray(16)); deleteOnExit() }
        val link = File(directory, "${UUID.randomUUID()}.pdu")
        try {
            java.nio.file.Files.createSymbolicLink(link.toPath(), external.toPath())
            assertTrue(store.hasBudget(512L * 1024 * 1024))
        } catch (_: UnsupportedOperationException) {
            // Some filesystems used by test runners do not support symbolic links.
        }
    }

    @Test
    fun successfulCallbackRecordsSentNotDeliveredAndDeletesVerifiedPdu() {
        val client = enrollAndUnlock()
        val command = queueCommand(client, SimRoutes.routeId(3), "group", listOf("+15551234567", "+15557654321"))
        assertEquals("mms", client.pendingCommands().single { it.commandId == command }.message.transport)
        assertEquals(uniffi.openpush_mobile_bindings.NativePermitState.PERMIT, client.beginSendAttempt(command).state)
        client.recordSendResult(command, NativeSendResult.SUBMITTED)
        val store = MmsCallbackStore(context)
        assertTrue(store.register(command))
        pdu(command).parentFile!!.mkdirs(); pdu(command).writeText("private plaintext pdu")

        assertTrue(store.record(command, NativeSendResult.SENT))
        assertTrue(MmsCallbackReconciler.apply(client, store, command, NativeSendResult.SENT))

        assertEquals("Sent", client.beginSendAttempt(command).existingSendState)
        assertFalse("confirmed core acceptance releases the PDU", pdu(command).exists())
        assertTrue(store.owed().isEmpty())
    }

    @Test
    fun transientTypedCoreErrorKeepsOwedEvidenceForReplay() {
        val client = enrollAndUnlock()
        val command = queueCommand(client, SimRoutes.routeId(3), "group", listOf("+15551234567", "+15557654321"))
        client.beginSendAttempt(command)
        val store = MmsCallbackStore(context)
        assertTrue(store.register(command))
        assertTrue(store.record(command, NativeSendResult.SENT))
        NativeGateway.closeForTest()

        assertFalse(MmsCallbackReconciler.apply(client, store, command, NativeSendResult.SENT))
        assertEquals(listOf(command to NativeSendResult.SENT), store.owed())
    }


    @Test
    fun permanentTypedCoreRefusalSettlesOwedEvidence() {
        val client = enrollAndUnlock()
        val command = queueCommand(client, SimRoutes.routeId(3), "group", listOf("+15551234567", "+15557654321"))
        client.beginSendAttempt(command)
        client.recordSendResult(command, NativeSendResult.FAILED_BEFORE_SUBMISSION)
        val store = MmsCallbackStore(context)
        assertTrue(store.register(command)); assertTrue(store.record(command, NativeSendResult.SENT))

        assertFalse(MmsCallbackReconciler.apply(client, store, command, NativeSendResult.SENT))
        assertTrue("InvalidRequest is a permanent illegal transition", store.owed().isEmpty())
    }

    @Test
    fun unknownThenLateSuccessIsAppliedOnceAndNeverCausesAResend() {
        val client = enrollAndUnlock()
        val command = queueCommand(client, SimRoutes.routeId(3), "group", listOf("+15551234567", "+15557654321"))
        client.beginSendAttempt(command)
        client.recordSendResult(command, NativeSendResult.SUBMITTED)
        val store = MmsCallbackStore(context)
        assertTrue(store.register(command))

        assertNull(MmsStatusReceiver.handle(context, callback(command), 12_345))
        assertEquals("SubmittedToOs", client.beginSendAttempt(command).existingSendState)
        assertEquals(NativeSendResult.SENT, MmsStatusReceiver.handle(context, callback(command), Activity.RESULT_OK))
        assertEquals("Sent", client.beginSendAttempt(command).existingSendState)
        assertNull("late duplicates do not create another outcome", MmsStatusReceiver.handle(context, callback(command), Activity.RESULT_OK))
        assertEquals(uniffi.openpush_mobile_bindings.NativePermitState.ALREADY_ATTEMPTED, client.beginSendAttempt(command).state)
    }

    @Test
    fun receiverReadsOrderedResultCodeBeforeGoAsync() {
        val client = enrollAndUnlock()
        val command = queueCommand(client, SimRoutes.routeId(3), "group", listOf("+15551234567", "+15557654321"))
        client.beginSendAttempt(command)
        client.recordSendResult(command, NativeSendResult.SUBMITTED)
        assertTrue(MmsCallbackStore(context).register(command))
        val previous: Executor = ReceiverWork.executor
        ReceiverWork.executor = Executor { it.run() }
        try {
            val receiver = MmsStatusReceiver()
            val pending = android.content.BroadcastReceiver.PendingResult::class.java.declaredConstructors.single { it.parameterCount == 9 }.let { constructor ->
                constructor.isAccessible = true
                constructor.newInstance(Activity.RESULT_OK, null, null, 0, true, false, null, 0, 0)
            }
            android.content.BroadcastReceiver::class.java
                .getDeclaredMethod("setPendingResult", android.content.BroadcastReceiver.PendingResult::class.java)
                .invoke(receiver, pending)
            receiver.onReceive(context, callback(command))
        } finally {
            ReceiverWork.executor = previous
        }
        assertEquals("Sent", client.beginSendAttempt(command).existingSendState)
    }
}
