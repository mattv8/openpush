package dev.openpush.mobile

import android.net.Uri
import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import uniffi.openpush_mobile_bindings.NativeMmsAcquisition
import uniffi.openpush_mobile_bindings.NativeMmsAcquisitionInput
import uniffi.openpush_mobile_bindings.NativeMmsAcquisitionState

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class MmsCaptureOrchestrationTest {
    @Test fun beginFailureDoesNotAdvanceProviderCheckpointButPartialRowGetsDurableIdentity() {
        val provider = FakeProvider(mutableListOf(partialRecord(1)))
        val failing = FakeCore().apply { failBegin = true; checkpoint(false, "0") }

        MmsCapture.run(plan(), failing, provider, temp())
        assertEquals("0", failing.checkpoint(false))

        val core = FakeCore().apply { checkpoint(false, "0") }
        val result = MmsCapture.run(plan(), core, provider, temp())
        assertEquals("1", core.checkpoint(false))
        assertEquals("android-subscription-7", core.begun.single().source.subscriptionId)
        assertEquals(NativeMmsAcquisitionState.PENDING, core.states.single().second)
        assertEquals(1, result.pending)
    }

    @Test fun liveScanContinuesWhileHistoryUsesEnableTimeFrozenBaseline() {
        val core = FakeCore().apply { checkpoint(false, "10"); checkpoint(true, "0") }
        val provider = FakeProvider(mutableListOf(readyRecord(2), readyRecord(11)))
        var completed = false
        var savedBaseline: Long? = null
        val plan = plan(importHistory = true, historyBaseline = 5, finishHistory = { completed = true }, saveHistoryBaseline = { savedBaseline = it })

        MmsCapture.run(plan, core, provider, temp())
        MmsCapture.run(plan, core, provider, temp())

        assertEquals("11", core.checkpoint(false))
        assertEquals("2", core.checkpoint(true))
        assertNull(savedBaseline)
        assertTrue(completed)
        assertTrue(core.begun.any { !it.imported && it.source.providerMessageId == "11" })
        assertTrue(core.begun.any { it.imported && it.source.providerMessageId == "2" })
    }

    @Test fun firstRunFreezesHistoryBaselineEvenWhenHistoryImportIsOff() {
        val core = FakeCore()
        val provider = FakeProvider(mutableListOf(readyRecord(12)))
        var baseline: Long? = null

        MmsCapture.run(plan(saveHistoryBaseline = { baseline = it }), core, provider, temp())

        assertEquals(12L, baseline)
        assertEquals("12", core.checkpoint(false))
        assertEquals(emptyList<NativeMmsAcquisitionInput>(), core.begun)
    }

    @Test fun firstLiveObservationCannotBecomeImportedDuringOverlappingHistoryScan() {
        val core = FakeCore().apply { checkpoint(false, "0"); checkpoint(true, "0") }
        val provider = FakeProvider(mutableListOf(readyRecord(5)))
        MmsCapture.run(plan(importHistory = true, historyBaseline = 10), core, provider, temp())
        assertFalse(core.acquisitions.getValue("5").input.imported)
    }

    @Test fun knownOtherSimCapturesInOwnContextWithoutChangingSendRoute() {
        val otherSim = readyRecord(4).copy(subscriptionId = 8)
        val core = FakeCore().apply { checkpoint(false, "0") }

        MmsCapture.run(plan(), core, FakeProvider(mutableListOf(otherSim)), temp())

        assertEquals("4", core.checkpoint(false))
        assertEquals("android-subscription-8", core.begun.single().source.subscriptionId)
    }

    @Test fun unknownSubscriptionBlocksCheckpointButLaterKnownRowsAreAttempted() {
        val unknown = readyRecord(4).copy(subscriptionId = null)
        val later = readyRecord(5).copy(subscriptionId = 8)
        val core = FakeCore().apply { checkpoint(false, "0") }

        MmsCapture.run(plan(), core, FakeProvider(mutableListOf(unknown, later)), temp())

        assertEquals("0", core.checkpoint(false))
        assertTrue(core.begun.none { it.source.providerMessageId == "4" })
        assertTrue(core.begun.any { it.source.providerMessageId == "5" && it.source.subscriptionId == "android-subscription-8" })
    }

    @Test fun streamBackedTextAndTransactionIdPersistAfterDurableIdentity() {
        val part = MmsProviderPart(31, 0, "text/plain", null, 106, null, Uri.parse("content://mms/part/31"))
        val record = readyRecord(3, listOf(part)).copy(transactionId = "op-123")
        val provider = FakeProvider(mutableListOf(record)).apply { text[31] = "stream body" }
        val core = FakeCore().apply { checkpoint(false, "0") }

        val first = MmsCapture.run(plan(), core, provider, temp())
        assertTrue(first.more)
        assertEquals("", core.begun.single().body)
        assertEquals("op-123", core.begun.single().transactionId)

        MmsCapture.run(plan(), core, provider, temp())
        assertEquals("stream body", core.begun.last().body)
        assertEquals("op-123", core.begun.last().transactionId)
        assertTrue(core.completed.isNotEmpty())
    }

    @Test fun sendRequestStaysOutgoingPendingUntilSameProviderRowBecomesSent() {
        val outbox = readyRecord(6).copy(messageType = 128, messageBox = 4, transactionId = "op-command", sender = emptyList(), toRecipients = listOf("+15550002"))
        val provider = FakeProvider(mutableListOf(outbox))
        val core = FakeCore().apply { checkpoint(false, "0") }

        val first = MmsCapture.run(plan(), core, provider, temp())
        assertFalse(core.begun.single().incoming)
        assertEquals("provider_box_pending", core.states.last().third)
        assertTrue(core.completed.isEmpty())
        assertEquals(1, first.pending)

        provider.records[0] = outbox.copy(messageBox = 2)
        MmsCapture.run(plan(), core, provider, temp())
        MmsCapture.run(plan(), core, provider, temp())
        assertTrue(core.begun.all { !it.incoming })
        assertTrue(core.completed.isNotEmpty())
        assertEquals(1, core.acquisitions.size)
    }

    @Test fun readyDraftNeverCompletesAndBccNeverEntersReplyRecipients() {
        val draft = readyRecord(7).copy(messageType = 128, messageBox = 3, sender = emptyList(), toRecipients = listOf("+15550002"), bccRecipients = listOf("+15559999"))
        val core = FakeCore().apply { checkpoint(false, "0") }
        MmsCapture.run(plan(), core, FakeProvider(mutableListOf(draft)), temp())
        assertEquals(listOf("+15550002"), core.begun.single().recipients)
        assertTrue(core.completed.isEmpty())
    }

    @Test fun outgoingBccOnlyRemainsBlockedWithoutLeakingRecipient() {
        val sent = readyRecord(8).copy(messageType = 128, messageBox = 2, sender = emptyList(), toRecipients = emptyList(), ccRecipients = emptyList(), bccRecipients = listOf("+15559999"))
        val core = FakeCore().apply { checkpoint(false, "0") }
        MmsCapture.run(plan(), core, FakeProvider(mutableListOf(sent)), temp())
        assertTrue(core.begun.single().recipients.isEmpty())
        assertEquals("bcc_only_recipients_not_modeled", core.states.last().third)
        assertTrue(core.completed.isEmpty())
    }

    @Test fun pendingRechecksAreCappedAtFifty() {
        val core = FakeCore().apply {
            checkpoint(false, "100")
            (1L..60L).forEach { id -> acquisitions["$id"] = acquisition(inputFor(id), NativeMmsAcquisitionState.PENDING, null) }
        }
        MmsCapture.run(plan(), core, FakeProvider(mutableListOf()), temp())
        assertEquals(50, core.states.count { it.third == "provider_row_missing" })
    }

    @Test fun unavailableRowsDoNotCreateImmediateContinuationLoop() {
        val core = FakeCore().apply {
            checkpoint(false, "9")
            acquisitions["9"] = acquisition(inputFor(9), NativeMmsAcquisitionState.PENDING, null)
        }
        val result = MmsCapture.run(plan(), core, FakeProvider(mutableListOf()), temp())
        assertFalse(result.more)
        assertEquals(1, result.unavailable)
    }

    @Test fun terminalRowsDoNotStarveNewPendingAcquisition() {
        val core = FakeCore().apply {
            checkpoint(false, "100")
            (1L..60L).forEach { id -> acquisitions["$id"] = acquisition(inputFor(id), NativeMmsAcquisitionState.UNAVAILABLE, "provider_row_missing") }
            acquisitions["61"] = acquisition(inputFor(61), NativeMmsAcquisitionState.PENDING, null)
        }
        val provider = FakeProvider(mutableListOf(readyRecord(61)))
        MmsCapture.run(plan(), core, provider, temp())
        MmsCapture.run(plan(), core, provider, temp())
        assertTrue(core.completed.contains("acq-61"))
    }

    @Test fun pendingRecheckCursorRotatesAcrossMoreThanFiftyRows() {
        val core = FakeCore().apply {
            checkpoint(false, "100")
            (1L..60L).forEach { id -> acquisitions["$id"] = acquisition(inputFor(id), NativeMmsAcquisitionState.PENDING, null) }
        }
        val provider = FakeProvider((1L..60L).map { readyRecord(it).copy(messageType = 128, messageBox = 4) }.toMutableList())
        var cursor: String? = null
        MmsCapture.run(plan(recheckCursor = null, saveRecheckCursor = { cursor = it }), core, provider, temp())
        MmsCapture.run(plan(recheckCursor = cursor, saveRecheckCursor = { cursor = it }), core, provider, temp())
        assertEquals((1L..60L).map { "acq-$it" }.toSet(), core.states.map { it.first }.toSet())
    }

    @Test fun manualRecheckRequeuesTerminalRowsButAutomaticPassDoesNot() {
        val core = FakeCore().apply {
            checkpoint(false, "100")
            acquisitions["70"] = acquisition(inputFor(70), NativeMmsAcquisitionState.UNAVAILABLE, "provider_row_missing")
            acquisitions["71"] = acquisition(inputFor(71), NativeMmsAcquisitionState.BLOCKED, "media_quota")
        }
        val provider = FakeProvider(mutableListOf(readyRecord(70), readyRecord(71)))
        MmsCapture.run(plan(), core, provider, temp())
        assertTrue(core.states.isEmpty())
        MmsCapture.run(plan(manualRecheck = true), core, provider, temp())
        assertTrue(core.states.any { it.first == "acq-70" && it.second == NativeMmsAcquisitionState.PENDING })
        assertTrue(core.states.any { it.first == "acq-71" && it.second == NativeMmsAcquisitionState.PENDING })
    }

    @Test fun conflictFromCompletePreservesAuthoritativeBlockedState() {
        val record = readyRecord(80)
        val provider = FakeProvider(mutableListOf(record))
        val core = FakeCore().apply { checkpoint(false, "0") }
        MmsCapture.run(plan(), core, provider, temp())
        core.completeFailure = {
            core.setState(it, NativeMmsAcquisitionState.BLOCKED, "core_conflict_reason")
            throw uniffi.openpush_mobile_bindings.MobileBindingsException.Conflict()
        }
        MmsCapture.run(plan(), core, provider, temp())
        assertEquals(NativeMmsAcquisitionState.BLOCKED, core.acquisitions.getValue("80").state)
        assertEquals("core_conflict_reason", core.acquisitions.getValue("80").reason)
        assertFalse(core.states.any { it.third == "parts_incomplete" })
    }

    @Test fun terminalAcquisitionReturnedByBeginSkipsBinaryCopyAndStaysTerminal() {
        val binary = MmsProviderPart(891, 0, "image/png", "blocked.png", null, null, Uri.parse("content://mms/part/891"))
        val record = readyRecord(89, listOf(binary)).copy(messageType = 128, messageBox = 2, transactionId = "op-ambiguous", sender = emptyList())
        val provider = FakeProvider(mutableListOf(record))
        val core = FakeCore().apply {
            checkpoint(false, "0")
            unavailableOnBeginTransaction = "op-ambiguous"
        }
        MmsCapture.run(plan(), core, provider, temp())
        assertEquals(NativeMmsAcquisitionState.UNAVAILABLE, core.acquisitions.getValue("89").state)
        assertTrue(core.prepared.isEmpty())
        assertFalse(core.states.any { it.first == "acq-89" && it.second == NativeMmsAcquisitionState.PENDING })
    }

    @Test fun completeAcquisitionReturnedByBeginSkipsBinaryCopy() {
        val binary = MmsProviderPart(901, 0, "image/png", "one.png", null, null, Uri.parse("content://mms/part/901"))
        val record = readyRecord(90, listOf(binary)).copy(messageType = 128, messageBox = 2, transactionId = "op-command", sender = emptyList())
        val provider = FakeProvider(mutableListOf(record))
        val core = FakeCore().apply {
            checkpoint(false, "0")
            completeOnBeginTransaction = "op-command"
        }
        MmsCapture.run(plan(), core, provider, temp())
        assertEquals(NativeMmsAcquisitionState.COMPLETE, core.acquisitions.getValue("90").state)
        assertTrue(core.prepared.isEmpty())
    }

    @Test fun quotaPressureRecoversAutomaticallyAfterUploadsDrain() {
        val binary = MmsProviderPart(911, 0, "image/png", "one.png", null, null, Uri.parse("content://mms/part/911"))
        val provider = FakeProvider(mutableListOf(readyRecord(91, listOf(binary))))
        val core = FakeCore().apply {
            checkpoint(false, "0")
            pendingBytes = 512L * 1024 * 1024
        }
        val plan = plan()
        val directory = temp()
        MmsCapture.run(plan, core, provider, directory)
        MmsCapture.run(plan, core, provider, directory)
        assertEquals(NativeMmsAcquisitionState.PENDING, core.acquisitions.getValue("91").state)
        assertEquals("media_quota", core.acquisitions.getValue("91").reason)
        assertTrue(core.prepared.isEmpty())
        core.pendingBytes = 0
        MmsCapture.run(plan, core, provider, directory)
        MmsCapture.run(plan, core, provider, directory)
        assertEquals(listOf("acq-91"), core.completed)
    }

    @Test fun manualRecheckDoesNotLeaveChangedOrUnknownSimPending() {
        for (subscription in listOf<Int?>(null, 8)) {
            val core = FakeCore().apply {
                checkpoint(false, "92")
                acquisitions["92"] = acquisition(inputFor(92), NativeMmsAcquisitionState.UNAVAILABLE, "previous_failure")
            }
            val provider = FakeProvider(mutableListOf(readyRecord(92).copy(subscriptionId = subscription)))
            MmsCapture.run(plan(manualRecheck = true), core, provider, temp())
            assertEquals(NativeMmsAcquisitionState.UNAVAILABLE, core.acquisitions.getValue("92").state)
            assertEquals("provider_subscription_changed", core.acquisitions.getValue("92").reason)
            assertTrue(core.completed.isEmpty())
        }
    }

    private fun plan(
        importHistory: Boolean = false,
        historyBaseline: Long? = null,
        saveHistoryBaseline: (Long) -> Unit = {},
        finishHistory: () -> Unit = {},
        recheckCursor: String? = null,
        saveRecheckCursor: (String?) -> Unit = {},
        manualRecheck: Boolean = false,
    ) = MmsCapturePlan("generation", SimRoute("android-subscription-7", 7, "send-only hint"), importHistory, historyBaseline, saveHistoryBaseline, finishHistory, recheckCursor, saveRecheckCursor, manualRecheck)

    private fun temp() = kotlin.io.path.createTempDirectory("mms-capture-test").toFile()
    private fun partialRecord(id: Long) = MmsProviderRecord(id, 4, 7, 1, 1_000, null, null, emptyList(), emptyList(), emptyList(), emptyList(), 130, null, emptyList(), false, "provider_content_pending")
    private fun readyRecord(id: Long, parts: List<MmsProviderPart> = listOf(MmsProviderPart(id * 10, 0, "text/plain", null, 106, "body$id", Uri.parse("content://mms/part/${id * 10}")))) =
        MmsProviderRecord(id, 4, 7, 1, 1_000, null, "subject", listOf("+15550001"), emptyList(), listOf("+15550002"), listOf("+15550003"), 132, null, parts, true, null)
    private fun inputFor(id: Long) = NativeMmsAcquisitionInput(
        uniffi.openpush_mobile_bindings.NativeMmsSource("generation", "android-subscription-7", "$id", "4"),
        true, "+15550001", listOf("+15550002"), null, "", false, 1_000, null,
    )
    private fun acquisition(input: NativeMmsAcquisitionInput, state: NativeMmsAcquisitionState, reason: String?) =
        NativeMmsAcquisition("acq-${input.source.providerMessageId}", "conversation", input, state, reason, emptyList())

    private class FakeProvider(val records: MutableList<MmsProviderRecord>) : MmsCaptureProvider {
        val text = mutableMapOf<Long, String>()
        override fun highWater(): Long = records.maxOfOrNull { it.id } ?: 0
        override fun page(afterId: Long, limit: Int): MmsProviderPage {
            val page = records.filter { it.id > afterId }.sortedBy { it.id }.take(limit)
            return MmsProviderPage(page, page.lastOrNull()?.id ?: afterId)
        }
        override fun read(id: Long): MmsProviderRecord? = records.firstOrNull { it.id == id }
        override fun readPartText(part: MmsProviderPart, byteLimit: Int): String? = part.text ?: text[part.providerPartId]
        override fun copyPart(part: MmsProviderPart, destination: File, byteLimit: Long): Long {
            destination.writeBytes(byteArrayOf(1, 2, 3))
            return 3
        }
    }

    private class FakeCore : MmsCaptureCore {
        val checkpoints = mutableMapOf<Pair<String, Boolean>, String?>()
        val acquisitions = linkedMapOf<String, NativeMmsAcquisition>()
        val begun = mutableListOf<NativeMmsAcquisitionInput>()
        val states = mutableListOf<Triple<String, NativeMmsAcquisitionState, String?>>()
        val completed = mutableListOf<String>()
        val prepared = mutableListOf<String>()
        var failBegin = false
        var completeOnBeginTransaction: String? = null
        var unavailableOnBeginTransaction: String? = null
        var completeFailure: ((String) -> Unit)? = null
        var pendingBytes = 0L
        fun checkpoint(imported: Boolean, value: String) { checkpoints[MmsCapture.PROVIDER_SCOPE to imported] = value }
        fun checkpoint(imported: Boolean) = checkpoints[MmsCapture.PROVIDER_SCOPE to imported]
        override fun acquisitions(limit: Int) = acquisitions.values.take(limit)
        override fun begin(input: NativeMmsAcquisitionInput): NativeMmsAcquisition {
            if (failBegin) error("store unavailable")
            begun += input
            val id = input.source.providerMessageId
            val old = acquisitions[id]
            val persistedInput = if (old == null) input else input.copy(imported = old.input.imported)
            val state = when {
                completeOnBeginTransaction != null && input.transactionId == completeOnBeginTransaction -> NativeMmsAcquisitionState.COMPLETE
                unavailableOnBeginTransaction != null && input.transactionId == unavailableOnBeginTransaction -> NativeMmsAcquisitionState.UNAVAILABLE
                else -> old?.state ?: NativeMmsAcquisitionState.PENDING
            }
            val reason = if (state == NativeMmsAcquisitionState.UNAVAILABLE) "transaction id reconciliation unavailable" else old?.reason
            val value = NativeMmsAcquisition("acq-$id", "conversation", persistedInput, state, reason, emptyList())
            acquisitions[id] = value
            return value
        }
        override fun setState(id: String, state: NativeMmsAcquisitionState, reason: String?) {
            states += Triple(id, state, reason)
            val entry = acquisitions.entries.first { it.value.acquisitionId == id }
            entry.setValue(entry.value.copy(state = state, reason = reason))
        }
        override fun checkpoint(generation: String, subscription: String, imported: Boolean) = checkpoints[subscription to imported]
        override fun setCheckpoint(generation: String, subscription: String, imported: Boolean, providerId: String) { checkpoints[subscription to imported] = providerId }
        override fun mappedParts(id: String) = emptyMap<String, String>()
        override fun pendingMediaBytes() = pendingBytes
        override fun prepareAttachment(path: String, mediaType: String, name: String): String {
            prepared += name
            return "attachment-${prepared.size}"
        }
        override fun mapPart(id: String, providerPartId: String, attachmentId: String) = Unit
        override fun discardAttachment(id: String) = Unit
        override fun complete(id: String) {
            completeFailure?.invoke(id)
            completed += id
        }
    }
}
