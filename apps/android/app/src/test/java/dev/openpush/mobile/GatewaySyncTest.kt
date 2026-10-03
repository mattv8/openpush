package dev.openpush.mobile

import org.json.JSONArray
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import uniffi.openpush_mobile_bindings.NativeClient
import uniffi.openpush_mobile_bindings.NativeIncomingSms
import uniffi.openpush_mobile_bindings.NativePermitState
import uniffi.openpush_mobile_bindings.NativeSnapshotProjectionState

/** A loopback stand-in for the server routes the worker uses; no carrier or real server involved. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class GatewaySyncTest : GatewayTestBase() {
    private val requests = mutableListOf<Pair<String, String>>()
    private var events = JSONArray()
    private var snapshot = JSONArray()
    private var eventsStatus = 200
    private var uploadResponse = LoopbackHttpServer.Response(200, """{"cursor":"9","duplicate":false}""")
    /** Responses consumed (one each) before normal snapshot-record pages are served. */
    private val brokenRecordPages = ArrayDeque<LoopbackHttpServer.Response>()
    private var unauthorized = false
    /** Overrides the `/v1/snapshot` cut as (high_water_cursor, record_count). */
    private var snapshotCut: Pair<String, String>? = null
    private var compactionCapabilityResponse = LoopbackHttpServer.Response(204)
    private var snapshotCompactionSupported = false
    private var snapshotCompactionActive = false

    private val server = LoopbackHttpServer { request ->
        synchronized(requests) { requests += "${request.method} ${request.target}" to request.body }
        val path = request.target.substringBefore('?')
        when {
            unauthorized || request.headers["authorization"] != "Bearer $TEST_TOKEN" -> LoopbackHttpServer.Response(401)
            request.method == "POST" && path == "/v1/capabilities" -> LoopbackHttpServer.Response(204)
            request.method == "POST" && path == "/v1/compaction/capability" -> compactionCapabilityResponse
            request.method == "GET" && path == "/v1/events" ->
                if (eventsStatus == 200) LoopbackHttpServer.Response(200, JSONObject().put("events", events).toString())
                else LoopbackHttpServer.Response(eventsStatus, """{"code":"resync_required"}""")
            request.method == "POST" && path == "/v1/events" -> uploadResponse
            request.method == "GET" && path == "/v1/snapshot" -> LoopbackHttpServer.Response(
                200,
                (snapshotCut ?: (snapshot.length().toString() to snapshot.length().toString())).let { (highWater, count) ->
                    JSONObject()
                        .put("high_water_cursor", highWater)
                        .put("record_count", count)
                        .put("compaction_supported", snapshotCompactionSupported)
                        .put("compaction_active", snapshotCompactionActive)
                        .apply { if (snapshotCompactionSupported) put("compaction_generation", "1") }
                        .toString()
                },
            )
            request.method == "GET" && path == "/v1/snapshot/records" -> brokenRecordPages.removeFirstOrNull()
                ?: LoopbackHttpServer.Response(200, JSONObject().put("records", recordsAfter(request.target)).toString())
            else -> LoopbackHttpServer.Response(404)
        }
    }

    private fun recordsAfter(target: String): JSONArray {
        val after = Regex("after=([0-9]+)").find(target)!!.groupValues[1].toInt()
        val limit = Regex("limit=([0-9]+)").find(target)!!.groupValues[1].toInt()
        return JSONArray().apply { for (i in after until minOf(after + limit, snapshot.length())) put(snapshot.get(i)) }
    }

    @After
    fun stop() = server.close()

    private val state get() = GatewayStateStore(context)
    private val routes = SimRoutes.current(3)

    private fun record(cursor: Int, wire: String) = JSONObject().put("cursor", cursor.toString()).put("envelope", JSONObject(wire))

    private fun sync(
        client: NativeClient,
        carrier: FakeCarrier = FakeCarrier(),
        media: () -> MmsTransferResult = { MmsTransferResult(false, 0, 0, emptyList()) },
        contacts: () -> Boolean = { false },
    ) = GatewaySync(
        client, GatewayHttp(server.origin, TEST_TOKEN), state, deviceId,
        dispatch = { SmsDispatcher(context, client, carrier, routes, sendPermissionGranted = true).dispatch() },
        capabilities = SimRoutes.capabilityReport(routes, sendPermissionGranted = true),
        media = media,
        contacts = contacts,
    )

    private fun uploads() = requests.count { it.first == "POST /v1/events" }

    @Test
    fun freshEnrollmentBootstrapsFromSnapshotSoHistoricalCommandsNeverReachTheCarrier() {
        val client = enrollAndUnlock()
        assertEquals(SyncPhase.BOOTSTRAP, state.phase)
        val (historical, wire) = desktopCommandEnvelope(SimRoutes.routeId(3), "already handled by someone")
        snapshot = JSONArray().put(record(1, wire))
        events = JSONArray().put(record(1, wire)) // a live replay from 0 would deliver it as live
        client.captureIncoming(NativeIncomingSms(null, "+15550003333", "captured during bootstrap", "p-boot", false))
        val carrier = FakeCarrier()

        val first = sync(client, carrier).run()
        assertNull(first.failure)
        assertTrue("never replays live history at cursor 0", requests.none { it.first.startsWith("GET /v1/events?after=0") })
        assertEquals(SyncPhase.LIVE, state.phase)
        assertTrue(carrier.calls.isEmpty())
        assertEquals(NativePermitState.HISTORICAL, client.beginSendAttempt(historical).state)
        assertEquals("1", client.receiveCursor())
        assertTrue("the bootstrap capture uploads only after the decision", uploads() >= 1)
    }

    @Test
    fun oldHistoryWithOwnProducerRecordsEntersDurableRecoveryWithoutUploadOrCarrier() {
        val client = enrollAndUnlock()
        client.captureIncoming(NativeIncomingSms(null, "+15550004444", "an earlier install's capture", "p-old", false))
        val ownWire = client.pendingOutboxJsonBatch(1uL).single() // producer_device_id == this device
        val (command, commandWire) = desktopCommandEnvelope(SimRoutes.routeId(3), "old command")
        snapshot = JSONArray().put(record(1, commandWire)).put(record(2, ownWire))
        val carrier = FakeCarrier()

        sync(client, carrier).run()
        assertEquals(SyncPhase.RECOVERY_REQUIRED, state.phase)
        assertEquals(RecoveryReason.REUSED_DEVICE_IDENTITY, state.recoveryReason)
        assertEquals(0, uploads())
        assertTrue(carrier.calls.isEmpty())

        requests.clear()
        events = JSONArray().put(record(1, commandWire))
        sync(client, carrier).run()
        assertEquals("durable across passes", SyncPhase.RECOVERY_REQUIRED, state.phase)
        assertEquals(0, uploads())
        assertTrue(carrier.calls.isEmpty())
        assertTrue(requests.none { it.first.startsWith("GET /v1/events") })
        assertEquals(
            "routes are withdrawn",
            SimRoutes.capabilityReport(emptyList(), false).toString(),
            requests.single { it.first == "POST /v1/capabilities" }.second,
        )
        assertTrue(client.beginSendAttempt(command).state != NativePermitState.PERMIT)
        assertEquals(SyncPhase.RECOVERY_REQUIRED, NativeGateway.status(context).syncPhase)
    }

    private fun live(): NativeClient = enrollAndUnlock().also { state.advance(SyncPhase.LIVE) }

    @Test
    fun declaresCompactionCapabilityBeforeProbingSnapshot() {
        sync(live()).run()
        assertTrue(
            requests.indexOfFirst { it.first == "POST /v1/compaction/capability" } <
                requests.indexOfFirst { it.first == "GET /v1/snapshot" },
        )
    }

    @Test
    fun missingCompactionCapabilityKeepsLegacySyncWorking() {
        compactionCapabilityResponse = LoopbackHttpServer.Response(404)
        val client = live()

        val result = sync(client).run()

        assertNull(result.failure)
        assertFalse(client.serverCompactionSupported())
        assertTrue(requests.any { it.first == "GET /v1/events?after=0&limit=50" })
    }

    @Test
    fun contactRepairOnLegacyServerDoesNotBlockSmsRelay() {
        val client = live()
        // The same durable latch is set when an existing database upgrades.
        client.requestContactRepair()
        compactionCapabilityResponse = LoopbackHttpServer.Response(404)
        val (_, wire) = desktopCommandEnvelope(SimRoutes.routeId(3), "SMS must still send")
        events = JSONArray().put(record(1, wire))
        client.captureIncoming(NativeIncomingSms(null, "+15550005555", "inbound during contact repair", "repair-inbound", false))
        val carrier = FakeCarrier()
        var contactCalls = 0

        val result = sync(client, carrier, contacts = { contactCalls++; false }).run()

        assertNull(result.failure)
        assertEquals(1, carrier.calls.size)
        assertTrue(uploads() > 0)
        assertTrue(requests.any { it.first.startsWith("GET /v1/events") })
        assertTrue(client.contactRepairRequired())
        assertEquals(0, contactCalls)
    }

    @Test
    fun unreadSmsIsReceivedLiveBeforeContactRepairSnapshot() {
        val client = live()
        client.requestContactRepair()
        snapshotCompactionSupported = true
        snapshotCompactionActive = true
        val (_, wire) = desktopCommandEnvelope(SimRoutes.routeId(3), "new command before contact repair")
        events = JSONArray().put(record(1, wire))
        snapshot = JSONArray().put(record(1, wire))
        val carrier = FakeCarrier()

        val result = sync(client, carrier).run()

        assertNull(result.failure)
        assertEquals(1, carrier.calls.size)
        assertFalse(client.contactRepairRequired())
    }

    @Test
    fun manualContactRepairUsesEmptyAuthoritativeSnapshotAndClearsTheLatch() {
        val client = live()
        client.requestContactRepair()
        snapshotCompactionSupported = true
        snapshotCompactionActive = true

        val result = sync(client).run()

        assertNull(result.failure)
        assertFalse(client.contactRepairRequired())
        assertEquals(NativeSnapshotProjectionState.PROMOTED, client.snapshotProjectionStatus()?.state)
    }

    @Test
    fun partialContactRepairSnapshotBlocksContactCallback() {
        val client = live()
        client.requestContactRepair()
        snapshotCompactionSupported = true
        snapshotCompactionActive = true
        val (_, wire) = desktopCommandEnvelope(SimRoutes.routeId(3), "staged repair history")
        snapshot = JSONArray().apply { repeat(801) { put(record(it + 1, wire)) } }
        var contactCalls = 0

        val result = sync(client, contacts = { contactCalls++; false }).run()

        assertNull(result.failure)
        assertTrue(result.more)
        assertTrue(client.contactRepairRequired())
        assertEquals(0, contactCalls)
    }

    @Test
    fun failedContactRepairProjectionDoesNotRunContactCallback() {
        val client = live()
        client.requestContactRepair()
        snapshotCompactionSupported = true
        snapshotCompactionActive = true
        val (_, wire) = desktopCommandEnvelope(SimRoutes.routeId(3), "corrupt repair history")
        val desktop = SharedVault.desktop
        desktop.setServerCompactionState(true, true)
        val producer = JSONObject(wire).getString("producer_device_id")
        desktop.captureContactBook(JSONObject().put("schema_version", 1)
            .put("book", JSONObject().put("id", "repair-book").put("owner_device_id", producer).put("generation", "1").put("state", "active"))
            .put("contacts", JSONArray()).toString())
        val contactEvent = desktop.pendingOutboxJson().map(::JSONObject).last { it.has("compaction") }
        desktop.ackOutbox(contactEvent.getString("envelope_id"))
        // A malformed historical command can be quarantined without invalidating contacts.
        // A corrupt compactable contact record must fail the authoritative projection.
        val (_, sms) = desktopCommandEnvelope(SimRoutes.routeId(3), "SMS after failed contact repair")
        client.ingestRaw(sms.toByteArray(), "1")
        client.applyPending(50uL)
        val corrupt = contactEvent.put("ciphertext", "AAAA").toString()
        snapshot = JSONArray().put(record(1, sms)).put(record(2, corrupt))
        events = JSONArray().put(record(2, corrupt))
        client.captureIncoming(NativeIncomingSms(null, "+15550005555", "upload despite repair failure", "failed-repair-inbound", false))
        val carrier = FakeCarrier()
        var contactCalls = 0

        val result = sync(client, carrier, contacts = { contactCalls++; false }).run()

        assertTrue(result.failure is GatewayTransportException)
        assertTrue(client.contactRepairRequired())
        assertEquals(NativeSnapshotProjectionState.FAILED, client.snapshotProjectionStatus()?.state)
        assertEquals(0, contactCalls)
        assertEquals(1, carrier.calls.size)
        assertTrue(uploads() > 0)
    }

    @Test
    fun receiveFailureDoesNotBlockUpload() {
        val client = live()
        client.captureIncoming(NativeIncomingSms(null, "+15550005555", "must still upload", "p-5xx", false))
        eventsStatus = 503
        val result = sync(client).run()
        assertNotNull("transient failure is reported for retry", result.failure)
        assertEquals(1, uploads())
        assertTrue(client.pendingOutboxJsonBatch(10uL).isEmpty())
    }

    @Test
    fun transientMediaFailureStillUploadsAndDispatchesSmsAfterReceive() {
        val client = live()
        client.captureIncoming(NativeIncomingSms(null, "+15550009991", "unrelated envelope", "p-media", false))
        val (commandId, wire) = desktopCommandEnvelope(SimRoutes.routeId(3), "unrelated SMS")
        events = JSONArray().put(record(1, wire))
        val carrier = FakeCarrier()
        val phases = mutableListOf<String>()
        val sync = GatewaySync(
            client, GatewayHttp(server.origin, TEST_TOKEN), state, deviceId,
            dispatch = {
                phases += "dispatch"
                SmsDispatcher(context, client, carrier, routes, sendPermissionGranted = true).dispatch()
            },
            capabilities = SimRoutes.capabilityReport(routes, sendPermissionGranted = true),
            media = {
                phases += "media"
                MmsTransferResult(false, 0, 0, listOf(MmsTransferFailure("00000000-0000-0000-0000-000000000001", "upload", "transient")))
            },
        )

        val result = sync.run()

        assertNotNull("transient media failure still requests backoff", result.failure)
        assertEquals(listOf("media", "dispatch"), phases)
        assertTrue("receive completed before media", requests.any { it.first == "GET /v1/events?after=0&limit=50" })
        assertTrue("unrelated outbox upload is not starved", uploads() >= 1)
        assertEquals(commandId, CallbackIdentity.parse(shadowOf(carrier.calls.single().sent.first()).savedIntent.data)!!.commandId)
    }

    @Test
    fun mediaAuthFailureAbortsBeforeUploadOrCarrierDispatch() {
        val client = live()
        client.captureIncoming(NativeIncomingSms(null, "+15550009992", "must remain queued", "p-media-auth", false))
        var dispatched = false
        var thrown: Throwable? = null
        try {
            GatewaySync(
                client, GatewayHttp(server.origin, TEST_TOKEN), state, deviceId,
                dispatch = { dispatched = true; DispatchSummary(0, 0, 0, false) },
                capabilities = SimRoutes.capabilityReport(routes, sendPermissionGranted = true),
                media = { MmsTransferResult(false, 0, 0, listOf(MmsTransferFailure("00000000-0000-0000-0000-000000000002", "upload", "auth"))) },
            ).run()
        } catch (error: GatewayAuthException) {
            thrown = error
        }
        assertTrue(thrown is GatewayAuthException)
        assertFalse(dispatched)
        assertEquals(0, uploads())
    }

    @Test
    fun bootstrapNeverRunsMediaPhase() {
        val client = enrollAndUnlock()
        var mediaCalls = 0
        sync(client, media = { mediaCalls++; MmsTransferResult(false, 0, 0, emptyList()) }).run()
        assertEquals(0, mediaCalls)
    }

    @Test
    fun impossibleSnapshotCutRestagesFromAFreshCut() {
        val client = live()
        val (_, wire) = desktopCommandEnvelope(SimRoutes.routeId(3), "resync history")
        snapshot = JSONArray().put(record(1, wire))
        eventsStatus = 409
        brokenRecordPages += LoopbackHttpServer.Response(409, """{"code":"resync_required"}""")
        val result = sync(client).run()
        assertNull(result.failure)
        // One capability probe plus the original and replacement snapshot cuts.
        assertEquals(3, requests.count { it.first == "GET /v1/snapshot" })
        assertNull("restaged generation finished", client.snapshotProgress())
        eventsStatus = 200
        sync(client).run()
        assertEquals("1", client.receiveCursor())
    }

    @Test
    fun repeatedImpossibleCutIsReportedForBoundedRetry() {
        val client = live()
        val (_, wire) = desktopCommandEnvelope(SimRoutes.routeId(3), "x")
        snapshot = JSONArray().put(record(1, wire))
        eventsStatus = 409
        repeat(2) { brokenRecordPages += LoopbackHttpServer.Response(200, """{"records":[]}""") }
        assertTrue(sync(client).run().failure is GatewayTransportException)
    }

    @Test
    fun uploadIdempotencyConflictEntersRecoveryAndStopsCarrierWork() {
        val client = live()
        val (_, wire) = desktopCommandEnvelope(SimRoutes.routeId(3), "would be sent")
        events = JSONArray().put(record(1, wire))
        client.captureIncoming(NativeIncomingSms(null, "+15550006666", "conflicting", "p-409", false))
        uploadResponse = LoopbackHttpServer.Response(409, """{"code":"idempotency_conflict"}""")
        val carrier = FakeCarrier()
        sync(client, carrier).run()
        assertEquals(SyncPhase.RECOVERY_REQUIRED, state.phase)
        assertEquals(RecoveryReason.PRODUCER_CONFLICT, state.recoveryReason)
        assertTrue(carrier.calls.isEmpty())
        assertTrue("never acked", client.pendingOutboxJsonBatch(10uL).isNotEmpty())
    }

    @Test
    fun permanentUploadRejectionIsDistinctTerminalAndNeverAcked() {
        val client = live()
        client.captureIncoming(NativeIncomingSms(null, "+15550007777", "rejected", "p-422", false))
        uploadResponse = LoopbackHttpServer.Response(422, """{"code":"invalid_envelope"}""")
        val carrier = FakeCarrier()
        val result = sync(client, carrier).run()
        assertNull(result.failure)
        assertFalse(result.more)
        assertEquals("invalid_envelope", state.outboxRejection)
        assertEquals(1, client.pendingOutboxJsonBatch(10uL).size)
        requests.clear()
        sync(client, carrier).run()
        assertEquals("no retry loop", 0, uploads())
        assertEquals("invalid_envelope", NativeGateway.status(context).outboxRejection)
    }

    @Test
    fun livePassReplaysUploadsAndDispatchesOnce() {
        val client = live()
        val (commandId, wire) = desktopCommandEnvelope(SimRoutes.routeId(3), "from desktop")
        events = JSONArray().put(record(1, wire))
        val carrier = FakeCarrier()
        val result = sync(client, carrier).run()
        assertNull(result.failure)
        assertEquals(SimRoutes.capabilityReport(routes, true).toString(), requests.first { it.first == "POST /v1/capabilities" }.second)
        assertTrue(requests.any { it.first == "GET /v1/events?after=0&limit=50" })
        val sent = shadowOf(carrier.calls.single().sent.first()).savedIntent
        assertEquals(commandId, CallbackIdentity.parse(sent.data)!!.commandId)
        assertTrue("SUBMITTED status uploaded and acked", client.pendingOutboxJsonBatch(10uL).isEmpty())
        assertEquals("1", client.receiveCursor())
    }

    @Test
    fun rejectedCredentialStopsWithoutCarrierWork() {
        val client = live()
        unauthorized = true
        val carrier = FakeCarrier()
        var thrown: Throwable? = null
        try {
            sync(client, carrier).run()
        } catch (error: GatewayAuthException) {
            thrown = error
        }
        assertTrue(thrown is GatewayAuthException)
        assertTrue(carrier.calls.isEmpty())
    }

    private fun assertInvalidCutStaysInBootstrap(highWater: String, count: String) {
        val client = enrollAndUnlock()
        val (command, wire) = desktopCommandEnvelope(SimRoutes.routeId(3), "historical, must not run live")
        events = JSONArray().put(record(1, wire)) // what a live replay from 0 would deliver
        client.captureIncoming(NativeIncomingSms(null, "+15550008888", "must not upload yet", "p-cut-$count", false))
        snapshotCut = highWater to count
        val carrier = FakeCarrier()
        repeat(2) {
            val result = sync(client, carrier).run()
            assertTrue("reported for bounded retry", result.failure is uniffi.openpush_mobile_bindings.MobileBindingsException.InvalidRequest)
            assertEquals(SyncPhase.BOOTSTRAP, state.phase)
        }
        assertTrue("no live replay", requests.none { it.first.startsWith("GET /v1/events") })
        assertEquals("no upload", 0, uploads())
        assertTrue("no OS call", carrier.calls.isEmpty())
        assertEquals(NativePermitState.NOT_RECEIVED, client.beginSendAttempt(command).state)
        assertEquals("0", client.receiveCursor())
    }

    @Test
    fun snapshotCountAboveCoreLimitNeverBypassesBootstrap() = assertInvalidCutStaysInBootstrap("200000", "100001")

    @Test
    fun snapshotCountAboveHighWaterNeverBypassesBootstrap() = assertInvalidCutStaysInBootstrap("2", "5")
}
