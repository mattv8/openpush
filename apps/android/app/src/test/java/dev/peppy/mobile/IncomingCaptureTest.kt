package dev.peppy.mobile

import android.Manifest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class IncomingCaptureTest : GatewayTestBase() {
    private val parts = listOf(
        IncomingSmsPart("+15557654321", "first half, ", 1_700_000_000_000),
        IncomingSmsPart("+15557654321", "second half", 1_700_000_000_000),
    )

    private fun allMessages() = checkNotNull(NativeGateway.open(context)).let { client ->
        client.listConversations().flatMap { client.messages(it.conversationId) }
    }

    @Test
    fun capturesDurablyBeforeSchedulingEvenWhileVaultIsLocked() {
        grant(Manifest.permission.RECEIVE_SMS)
        assertEquals(ImportResult.IMPORTED, enroll()) // database open, shared keys locked
        val seenAtSchedule = mutableListOf<List<String>>()
        GatewayScheduler.schedule = { seenAtSchedule += allMessages().map { it.body } }

        assertEquals(1, IncomingCapture.handle(context, parts))
        assertEquals("the concatenated message was already in the core when sync was scheduled", listOf(listOf("first half, second half")), seenAtSchedule)

        // A redelivered broadcast is a core duplicate, not a second message.
        IncomingCapture.handle(context, parts)
        assertEquals(1, allMessages().size)
    }

    @Test
    fun noPermissionOrNoEnrollmentCapturesAndSchedulesNothing() {
        deny(Manifest.permission.RECEIVE_SMS)
        enroll()
        assertEquals(0, IncomingCapture.handle(context, parts))
        assertTrue(allMessages().isEmpty())
        assertTrue(scheduled.isEmpty())
    }

    @Test
    fun notEnrolledSchedulesNothing() {
        grant(Manifest.permission.RECEIVE_SMS)
        assertEquals(0, IncomingCapture.handle(context, parts))
        assertTrue(scheduled.isEmpty())
    }

    @Test
    fun broadcastIdIsDeterministicAndRevealsNothing() {
        val id = IncomingCapture.broadcastId("+15557654321", 1L, "secret code 123")
        assertEquals(id, IncomingCapture.broadcastId("+15557654321", 1L, "secret code 123"))
        assertTrue(id != IncomingCapture.broadcastId("+15557654321", 2L, "secret code 123"))
        assertTrue(!id.contains("5557654321") && !id.contains("123"))
    }

    @Test
    fun receiverWorkFinishesOnlyAfterDurableCaptureAndScheduling() {
        grant(Manifest.permission.RECEIVE_SMS)
        enroll()
        val previous = ReceiverWork.executor
        val queued = mutableListOf<Runnable>()
        val events = mutableListOf<String>()
        ReceiverWork.executor = java.util.concurrent.Executor { queued += it }
        try {
            GatewayScheduler.schedule = { events += "schedule:${allMessages().size}" }
            ReceiverWork.run(finish = { events += "finish" }) { IncomingCapture.handle(context, parts) }
            assertTrue("nothing finishes before the owned executor runs", events.isEmpty())
            queued.single().run()
            assertEquals(listOf("schedule:1", "finish"), events)
        } finally {
            ReceiverWork.executor = previous
        }
    }
}
