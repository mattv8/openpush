package dev.openpush.mobile

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import androidx.core.content.ContextCompat
import androidx.work.BackoffPolicy
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import org.json.JSONException
import org.json.JSONObject
import uniffi.openpush_mobile_bindings.MobileBindingsException
import uniffi.openpush_mobile_bindings.NativeClientInterface
import uniffi.openpush_mobile_bindings.NativeRawSnapshotRecord
import uniffi.openpush_mobile_bindings.NativeSnapshotProgress
import uniffi.openpush_mobile_bindings.NativeSnapshotPurpose
import java.io.IOException
import java.util.concurrent.TimeUnit

object GatewayWork {
    const val NAME = "openpush-gateway-sync"
    const val PERIODIC_NAME = "openpush-gateway-periodic"

    private fun constraints() = Constraints.Builder().setRequiredNetworkType(NetworkType.CONNECTED).build()

    /** Unique, network-constrained. APPEND_OR_REPLACE keeps a follow-up when a run is active. */
    fun enqueue(context: Context) {
        WorkManager.getInstance(context).enqueueUniqueWork(
            NAME,
            ExistingWorkPolicy.APPEND_OR_REPLACE,
            OneTimeWorkRequestBuilder<GatewaySyncWorker>()
                .setConstraints(constraints())
                .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 30, TimeUnit.SECONDS)
                .build(),
        )
    }

    /**
     * Commands queued on other devices have no push wake in this checkpoint, so a 15-minute
     * (platform minimum) network-constrained periodic pass picks them up; Doze may defer it.
     */
    fun ensurePeriodic(context: Context) {
        WorkManager.getInstance(context).enqueueUniquePeriodicWork(
            PERIODIC_NAME,
            ExistingPeriodicWorkPolicy.KEEP,
            PeriodicWorkRequestBuilder<GatewaySyncWorker>(15, TimeUnit.MINUTES).setConstraints(constraints()).build(),
        )
    }
}

/** Server rejected the credential; retrying cannot help until the user re-imports. */
class GatewayAuthException : IOException("credential rejected")

/**
 * One bounded sync pass over authenticated HTTP. Phases are isolated: a receive or snapshot failure
 * does not prevent uploading captures, and vice versa; the first transient failure is reported so
 * the worker retries with backoff.
 *
 * Bootstrap: a fresh database never starts live replay at cursor 0 (which would deliver historical
 * commands as live). It stages the server snapshot first (the core imports those commands as
 * historical, so they can never obtain a carrier permit). Any snapshot record produced by this
 * device ID proves the credential was used by an earlier install: the gateway enters the durable
 * RECOVERY_REQUIRED state, sets the core restore guard, and never uploads or touches the carrier.
 * Nothing is uploaded and no command is executed until the bootstrap snapshot is fully drained.
 *
 * Budgets per pass: up to 4 snapshot pages (200 records) or one replay page (50 events), up to 4
 * apply batches of 50, up to 50 uploads before and after dispatch, at most 10 carrier commands.
 */
class GatewaySync(
    private val client: NativeClientInterface,
    private val http: GatewayHttp,
    private val state: GatewayStateStore,
    private val deviceId: String,
    private val dispatch: () -> DispatchSummary,
    private val capabilities: JSONObject,
) {
    class PassResult(val more: Boolean, val failure: Exception?)

    private var more = false
    private var failure: Exception? = null

    fun run(): PassResult {
        if (state.phase == SyncPhase.RECOVERY_REQUIRED) {
            // Withdraw routes so the desktop stops offering this gateway; nothing else.
            phase { publish(SimRoutes.capabilityReport(emptyList(), sendPermissionGranted = false)); false }
            return PassResult(false, failure)
        }
        phase { publish(capabilities); false }
        when (state.phase) {
            SyncPhase.BOOTSTRAP -> phase { bootstrap() }
            SyncPhase.BOOTSTRAP_DRAINING -> phase { finishBootstrap() }
            SyncPhase.LIVE -> {
                phase { drain() }
                phase { receive() }
                phase { drain() }
                phase { NotificationMirrorService.reconcile(); false }
                // Receive/apply precedes any OS effect and outbound upload. A disconnected listener
                // or revoked access leaves the durable core effect pending rather than completing it.
                phase { NotificationMirrorService.runPendingDismissals(); false }
            }
            SyncPhase.RECOVERY_REQUIRED -> Unit
        }
        if (mayEmit()) phase { upload() }
        if (mayEmit()) phase { dispatch().moreRemaining }
        if (mayEmit()) phase { upload() }
        return PassResult(more, failure)
    }

    private fun mayEmit() = state.phase == SyncPhase.LIVE && state.outboxRejection == null

    private inline fun phase(block: () -> Boolean) {
        try {
            if (block()) more = true
        } catch (error: GatewayAuthException) {
            throw error
        } catch (error: Exception) {
            val transient = error is IOException || error is JSONException ||
                error is MobileBindingsException || error is NumberFormatException
            if (!transient) throw error
            if (failure == null) failure = error
        }
    }

    private fun publish(report: JSONObject) {
        requireOk(http.postJson("/v1/capabilities", report.toString()))
    }

    private fun bootstrap(): Boolean {
        val progress = client.snapshotProgress() ?: try {
            beginSnapshot(NativeSnapshotPurpose.RESYNC)
        } catch (error: MobileBindingsException.InvalidRequest) {
            // InvalidRequest is generic: an unfinished drain of an already-published generation,
            // but also record_count > 100k, count > high water, or a malformed cut. Never infer
            // which. Drain (harmless either way), stay in BOOTSTRAP, and report for bounded retry;
            // a fresh generation is staged once begin succeeds.
            drain()
            throw error
        }
        return when (stage(progress, NativeSnapshotPurpose.RESYNC, detectOwnRecords = true)) {
            Stage.OWN_RECORDS -> {
                enterRecovery(RecoveryReason.REUSED_DEVICE_IDENTITY)
                false
            }
            Stage.FINISHED -> {
                state.advance(SyncPhase.BOOTSTRAP_DRAINING)
                finishBootstrap()
            }
            Stage.STAGING -> true
        }
    }

    private fun finishBootstrap(): Boolean {
        val remaining = drain()
        if (!remaining) state.advance(SyncPhase.LIVE)
        return remaining
    }

    private fun receive(): Boolean {
        client.snapshotProgress()?.let {
            stage(it, NativeSnapshotPurpose.RESYNC, detectOwnRecords = false)
            return true // a finished generation still has to drain
        }
        val response = http.get("/v1/events?after=${client.receiveCursor()}&limit=$EVENT_PAGE")
        if (response.code == 409) {
            val progress = try {
                beginSnapshot(NativeSnapshotPurpose.RESYNC)
            } catch (_: MobileBindingsException.InvalidRequest) {
                return true // A published generation is still draining; drain first, then retry.
            }
            stage(progress, NativeSnapshotPurpose.RESYNC, detectOwnRecords = false)
            return true
        }
        val events = JSONObject(checkBody(response)).getJSONArray("events")
        for (index in 0 until events.length()) {
            val event = events.getJSONObject(index)
            client.ingestRaw(event.getJSONObject("envelope").toString().toByteArray(Charsets.UTF_8), event.getString("cursor"))
        }
        return events.length() >= EVENT_PAGE
    }

    private enum class Stage { STAGING, FINISHED, OWN_RECORDS }

    private fun beginSnapshot(purpose: NativeSnapshotPurpose): NativeSnapshotProgress {
        val start = JSONObject(checkBody(http.get("/v1/snapshot")))
        return client.beginSnapshot(start.getString("high_water_cursor"), start.getString("record_count").toULong(), purpose)
    }

    /**
     * Stages up to [SNAPSHOT_PAGES] pages. An impossible cut (server 400/409, an empty page before
     * the expected count, or a page the core refuses) restarts staging from a fresh `/v1/snapshot`
     * at most once per pass; a second failure is reported for bounded WorkManager retry.
     */
    private fun stage(initial: NativeSnapshotProgress, purpose: NativeSnapshotPurpose, detectOwnRecords: Boolean): Stage {
        var progress = initial
        var restarted = false
        repeat(SNAPSHOT_PAGES) {
            if (progress.receivedRecords >= progress.expectedRecords) {
                client.finishSnapshot(progress.generation)
                return Stage.FINISHED
            }
            val response = http.get(
                "/v1/snapshot/records?high_water=${progress.highWater}&after=${progress.lastCursor}&limit=$SNAPSHOT_PAGE",
            )
            if (response.code == 401 || response.code == 403) throw GatewayAuthException()
            val page = if (response.ok) JSONObject(checkBody(response)).getJSONArray("records") else null
            val impossibleCut = response.code == 400 || response.code == 409 || page?.length() == 0
            if (!impossibleCut && page == null) throw GatewayTransportException("HTTP ${response.code}")
            val next = if (impossibleCut) null else {
                val records = (0 until page!!.length()).map { index ->
                    val row = page.getJSONObject(index)
                    val envelope = row.getJSONObject("envelope")
                    if (detectOwnRecords && envelope.optString("producer_device_id") == deviceId) return Stage.OWN_RECORDS
                    NativeRawSnapshotRecord(row.getString("cursor"), envelope.toString().toByteArray(Charsets.UTF_8))
                }
                try {
                    client.appendSnapshotRawPage(progress.generation, records)
                } catch (_: MobileBindingsException.InvalidRequest) {
                    null
                }
            }
            if (next == null) {
                if (restarted) throw GatewayTransportException("snapshot cut unavailable")
                restarted = true
                progress = beginSnapshot(purpose) // obsoletes the broken staging generation
            } else {
                progress = next
            }
        }
        if (progress.receivedRecords >= progress.expectedRecords) {
            client.finishSnapshot(progress.generation)
            return Stage.FINISHED
        }
        return Stage.STAGING
    }

    /** `applied == 0` is not completion; only `snapshotRemaining == 0` ends a snapshot drain. */
    private fun drain(): Boolean {
        repeat(APPLY_BATCHES) {
            val report = client.applyPending(APPLY_BATCH.toULong())
            val progressed = report.applied + report.quarantined + report.drained
            if (report.snapshotRemaining == 0uL && progressed < APPLY_BATCH.toULong()) return false
        }
        return true
    }

    private fun upload(): Boolean {
        val batch = client.pendingOutboxJsonBatch(OUTBOX_BATCH.toULong())
        for (envelope in batch) {
            val response = http.postJson("/v1/events", envelope)
            when {
                response.ok -> client.ackOutbox(JSONObject(envelope).getString("envelope_id"))
                response.code == 401 -> throw GatewayAuthException()
                response.code == 409 && response.errorCode == "idempotency_conflict" -> {
                    enterRecovery(RecoveryReason.PRODUCER_CONFLICT)
                    return false
                }
                response.code in 400..499 && response.code != 408 && response.code != 429 -> {
                    // Permanent: never acked (that would be a lie) and never retried in a loop.
                    state.rejectOutbox(response.errorCode ?: "http_${response.code}")
                    return false
                }
                else -> throw GatewayTransportException("HTTP ${response.code}")
            }
        }
        return batch.size >= OUTBOX_BATCH
    }

    /** Durable app state first, then (best effort) the core's own restore guard. */
    private fun enterRecovery(reason: RecoveryReason) {
        state.requireRecovery(reason)
        try {
            beginSnapshot(NativeSnapshotPurpose.RESTORE)
        } catch (_: Exception) {
            // The durable app state alone already blocks upload and carrier work.
        }
    }

    private fun requireOk(result: HttpResult) {
        if (result.code == 401 || result.code == 403) throw GatewayAuthException()
        if (!result.ok) throw GatewayTransportException("HTTP ${result.code}")
    }

    private fun checkBody(result: HttpResult): String {
        requireOk(result)
        return result.body ?: throw GatewayTransportException("empty body")
    }

    companion object {
        const val EVENT_PAGE = 50
        const val SNAPSHOT_PAGES = 4
        const val SNAPSHOT_PAGE = 200
        const val APPLY_BATCHES = 4
        const val APPLY_BATCH = 50
        const val OUTBOX_BATCH = 50
    }
}

class GatewaySyncWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    /** One-time and periodic passes share the core; never run two passes concurrently. */
    override suspend fun doWork(): Result = PASS.withLock { withContext(Dispatchers.IO) { pass() } }

    private fun pass(): Result {
        val callbacks = SmsCallbackStore(applicationContext)
        callbacks.purgeOlderThan(TimeUnit.DAYS.toMillis(7))
        // Callback aggregates owed to the core (crash between callback and core write).
        NativeGateway.open(applicationContext)?.let { SmsCallbackReconciler.replay(it, callbacks) }
        // Not enrolled, database unopenable, or shared vault locked: nothing can be synced yet.
        val session = NativeGateway.session(applicationContext) ?: return Result.success()
        val sendGranted = ContextCompat.checkSelfPermission(applicationContext, Manifest.permission.SEND_SMS) ==
            PackageManager.PERMISSION_GRANTED
        val routes = SimRoutes.current()
        val sync = GatewaySync(
            client = session.client,
            http = GatewayHttp(session.origin, session.bearerToken),
            state = GatewayStateStore(applicationContext),
            deviceId = session.deviceId,
            dispatch = {
                SmsDispatcher(applicationContext, session.client, AndroidSmsCarrier(applicationContext), routes, sendGranted, callbacks).dispatch()
            },
            capabilities = SimRoutes.capabilityReport(routes, sendGranted),
        )
        val result = try {
            sync.run()
        } catch (_: GatewayAuthException) {
            return Result.failure()
        }
        return when {
            result.failure != null -> if (runAttemptCount < MAX_ATTEMPTS) Result.retry() else Result.failure()
            else -> {
                if (result.more) GatewayWork.enqueue(applicationContext)
                Result.success()
            }
        }
    }

    private companion object {
        const val MAX_ATTEMPTS = 5
        val PASS = Mutex()
    }
}
