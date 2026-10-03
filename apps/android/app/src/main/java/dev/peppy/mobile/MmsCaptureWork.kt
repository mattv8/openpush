package dev.peppy.mobile

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.net.Uri
import androidx.core.content.ContextCompat
import androidx.work.BackoffPolicy
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.workDataOf
import androidx.work.WorkerParameters
import java.io.File
import java.security.MessageDigest
import java.util.concurrent.TimeUnit
import kotlin.math.min
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import uniffi.peppy_mobile_bindings.NativeClientInterface
import uniffi.peppy_mobile_bindings.NativeMmsAcquisition
import uniffi.peppy_mobile_bindings.NativeMmsAcquisitionInput
import uniffi.peppy_mobile_bindings.NativeMmsAcquisitionState
import uniffi.peppy_mobile_bindings.NativeMmsSource
import uniffi.peppy_mobile_bindings.MobileBindingsException

/** Offline-only reconciliation for provider rows. Network upload remains GatewayWork's concern. */
object MmsCaptureWork {
    const val NAME = "peppy-mms-capture"
    const val PERIODIC_NAME = "peppy-mms-capture-periodic"

    fun enqueue(context: Context) = schedule(context, ExistingWorkPolicy.KEEP, delayed = true, manualRecheck = false)
    fun refresh(context: Context) = schedule(context, ExistingWorkPolicy.REPLACE, delayed = false, manualRecheck = true)

    internal fun handleResult(context: Context, result: MmsCaptureSummary) {
        if (result.captured > 0) GatewayWork.enqueue(context)
        if (result.more) enqueue(context)
    }

    fun ensurePeriodic(context: Context) {
        WorkManager.getInstance(context).enqueueUniquePeriodicWork(
            PERIODIC_NAME,
            ExistingPeriodicWorkPolicy.KEEP,
            PeriodicWorkRequestBuilder<MmsCaptureWorker>(15, TimeUnit.MINUTES)
                .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 30, TimeUnit.SECONDS)
                .build(),
        )
    }

    private fun schedule(context: Context, policy: ExistingWorkPolicy, delayed: Boolean, manualRecheck: Boolean) {
        val request = OneTimeWorkRequestBuilder<MmsCaptureWorker>()
            .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 30, TimeUnit.SECONDS)
            .setInputData(workDataOf(MANUAL_RECHECK to manualRecheck))
            .apply { if (delayed) setInitialDelay(15, TimeUnit.SECONDS) }
            .build()
        WorkManager.getInstance(context).enqueueUniqueWork(NAME, policy, request)
    }

    internal const val MANUAL_RECHECK = "manual-mms-recheck"
}

class MmsCaptureWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    override suspend fun doWork(): Result = PASS.withLock {
        withContext(Dispatchers.IO) {
            val result = MmsCapture.run(applicationContext, inputData.getBoolean(MmsCaptureWork.MANUAL_RECHECK, false))
            MmsCaptureWork.handleResult(applicationContext, result)
            Result.success()
        }
    }

    private companion object { val PASS = Mutex() }
}

internal data class MmsCapturePlan(
    val generation: String,
    val route: SimRoute?,
    val importHistory: Boolean,
    val historyBaseline: Long?,
    val saveHistoryBaseline: (Long) -> Unit,
    val finishHistory: () -> Unit,
    val recheckCursor: String?,
    val saveRecheckCursor: (String?) -> Unit,
    val manualRecheck: Boolean,
)

internal interface MmsCaptureProvider {
    fun highWater(): Long
    fun page(afterId: Long, limit: Int): MmsProviderPage
    fun read(id: Long): MmsProviderRecord?
    fun readPartText(part: MmsProviderPart, byteLimit: Int): String?
    fun copyPart(part: MmsProviderPart, destination: File, byteLimit: Long): Long
}

internal interface MmsCaptureCore {
    fun acquisitions(limit: Int): List<NativeMmsAcquisition>
    fun begin(input: NativeMmsAcquisitionInput): NativeMmsAcquisition
    fun setState(id: String, state: NativeMmsAcquisitionState, reason: String?)
    fun checkpoint(generation: String, subscription: String, imported: Boolean): String?
    fun setCheckpoint(generation: String, subscription: String, imported: Boolean, providerId: String)
    fun mappedParts(id: String): Map<String, String>
    fun pendingMediaBytes(): Long
    fun prepareAttachment(path: String, mediaType: String, name: String): String
    fun mapPart(id: String, providerPartId: String, attachmentId: String)
    fun discardAttachment(id: String)
    fun complete(id: String)
}

/** Provider orchestration is injected so tests exercise checkpoint/core boundary ordering. */
internal object MmsCapture {
    private const val PAGE = 50
    internal const val PROVIDER_SCOPE = "android-mms-provider"
    private const val CORE_LIST = 1000
    private const val MAX_PENDING_RECHECKS = 50
    private const val PASS_BUDGET_NANOS = 10_000_000_000L
    private const val MAX_BYTES = 32L * 1024 * 1024
    private const val MAX_BINARY = 10
    private const val PENDING_MEDIA_LIMIT = 512L * 1024 * 1024

    fun run(context: Context, manualRecheck: Boolean = false): MmsCaptureSummary {
        val prefs = MmsPreferences(context)
        if (!prefs.enabled || !granted(context)) return MmsCaptureSummary(0, 0, 0, false)
        val native = NativeGateway.open(context) ?: return MmsCaptureSummary(0, 0, 0, false)
        val route = SimRoutes.current().singleOrNull()
        val reader = MmsProviderReader(context)
        val provider = object : MmsCaptureProvider {
            override fun highWater() = currentHighWater(context)
            override fun page(afterId: Long, limit: Int) = reader.page(afterId, limit)
            override fun read(id: Long) = reader.read(id)
            override fun readPartText(part: MmsProviderPart, byteLimit: Int) = reader.readPartText(part, byteLimit)
            override fun copyPart(part: MmsProviderPart, destination: File, byteLimit: Long) = reader.copyPart(part, destination, byteLimit)
        }
        val plan = MmsCapturePlan(
            prefs.sourceGeneration,
            route,
            prefs.importHistory,
            prefs.historyBaseline(),
            { prefs.setHistoryBaseline(it) },
            { prefs.completeHistoryImport() },
            prefs.recheckCursor(),
            { prefs.setRecheckCursor(it) },
            manualRecheck,
        )
        return run(plan, NativeCore(native), provider, context.noBackupFilesDir)
    }

    internal fun run(plan: MmsCapturePlan, core: MmsCaptureCore, provider: MmsCaptureProvider, tempDirectory: File): MmsCaptureSummary {
        var captured = 0
        var pending = 0
        var unavailable = 0
        var followup = false
        val deadline = System.nanoTime() + PASS_BUDGET_NANOS
        fun budgetExpired() = System.nanoTime() >= deadline

        val eligible = core.acquisitions(CORE_LIST).filter {
            it.state == NativeMmsAcquisitionState.PENDING ||
                (plan.manualRecheck && it.state in setOf(NativeMmsAcquisitionState.BLOCKED, NativeMmsAcquisitionState.UNAVAILABLE))
        }
        val cursorIndex = plan.recheckCursor?.let { cursor -> eligible.indexOfFirst { it.acquisitionId == cursor } } ?: -1
        val durable = if (eligible.isEmpty()) emptyList() else {
            (eligible.drop(cursorIndex + 1) + eligible.take(cursorIndex + 1)).take(MAX_PENDING_RECHECKS)
        }
        plan.saveRecheckCursor(durable.lastOrNull()?.acquisitionId)
        for (acquisition in durable) {
            val recheck = if (plan.manualRecheck && acquisition.state != NativeMmsAcquisitionState.PENDING) {
                core.setState(acquisition.acquisitionId, NativeMmsAcquisitionState.PENDING, "manual_recheck")
                acquisition.copy(state = NativeMmsAcquisitionState.PENDING, reason = "manual_recheck")
            } else {
                acquisition
            }
            if (budgetExpired()) { followup = true; break }
            val providerId = recheck.input.source.providerMessageId.toLongOrNull() ?: continue
            val record = provider.read(providerId)
            if (record == null) {
                core.setState(acquisition.acquisitionId, NativeMmsAcquisitionState.UNAVAILABLE, "provider_row_missing")
                unavailable++
                continue
            }
            val routeId = record.subscriptionId?.takeIf { it >= 0 }?.let(SimRoutes::routeId)
            if (routeId == null || routeId != acquisition.input.source.subscriptionId) {
                core.setState(acquisition.acquisitionId, NativeMmsAcquisitionState.UNAVAILABLE, "provider_subscription_changed")
                unavailable++
                continue
            }
            val outcome = try {
                process(core, provider, tempDirectory, plan.generation, routeId, record, recheck.input.imported, recheck)
            } catch (_: Exception) {
                ProcessResult(durable = true)
            }
            captured += outcome.captured
            pending += outcome.pending
            unavailable += outcome.unavailable
            followup = followup || outcome.followup
        }

        val frozenBaseline = plan.historyBaseline ?: provider.highWater().also(plan.saveHistoryBaseline)
        var liveCheckpoint = core.checkpoint(plan.generation, PROVIDER_SCOPE, false)?.toLongOrNull()
        if (liveCheckpoint == null) {
            liveCheckpoint = frozenBaseline
            core.setCheckpoint(plan.generation, PROVIDER_SCOPE, false, liveCheckpoint.toString())
        }
        val live = scan(core, provider, tempDirectory, plan.generation, imported = false, after = liveCheckpoint, upperBound = null, ::budgetExpired)
        captured += live.captured
        pending += live.pending
        unavailable += live.unavailable
        followup = followup || live.more

        if (plan.importHistory && !budgetExpired()) {
            val historyCheckpoint = core.checkpoint(plan.generation, PROVIDER_SCOPE, true)?.toLongOrNull() ?: 0L
            val history = scan(core, provider, tempDirectory, plan.generation, imported = true, after = historyCheckpoint, upperBound = frozenBaseline, ::budgetExpired)
            captured += history.captured
            pending += history.pending
            unavailable += history.unavailable
            followup = followup || history.more
            if (history.finished) plan.finishHistory()
        }
        return MmsCaptureSummary(captured, pending, unavailable, followup)
    }

    private fun scan(
        core: MmsCaptureCore,
        provider: MmsCaptureProvider,
        tempDirectory: File,
        generation: String,
        imported: Boolean,
        after: Long,
        upperBound: Long?,
        budgetExpired: () -> Boolean,
    ): ScanResult {
        val page = provider.page(after, PAGE)
        var captured = 0
        var pending = 0
        var unavailable = 0
        var followup = false
        var processed = 0
        var stopped = false
        var checkpointBlocked = false
        for (record in page.records) {
            if (upperBound != null && record.id > upperBound) { stopped = true; break }
            if (budgetExpired()) { followup = true; stopped = true; break }
            val subscriptionId = record.subscriptionId?.takeIf { it >= 0 }
            if (subscriptionId == null) {
                pending++
                checkpointBlocked = true
                continue
            }
            val routeId = SimRoutes.routeId(subscriptionId)
            val outcome = try {
                process(core, provider, tempDirectory, generation, routeId, record, imported, null)
            } catch (_: Exception) {
                ProcessResult(durable = false)
            }
            captured += outcome.captured
            pending += outcome.pending
            unavailable += outcome.unavailable
            followup = followup || outcome.followup
            if (!outcome.durable) {
                checkpointBlocked = true
                continue
            }
            if (!checkpointBlocked) core.setCheckpoint(generation, PROVIDER_SCOPE, imported, record.id.toString())
            processed++
        }
        val exhausted = page.records.size < PAGE || stopped || (upperBound != null && page.records.any { it.id > upperBound })
        val finished = upperBound != null && exhausted && !followup && !checkpointBlocked
        return ScanResult(captured, pending, unavailable, followup || (!checkpointBlocked && !exhausted && processed == PAGE), finished)
    }

    private fun process(
        core: MmsCaptureCore,
        provider: MmsCaptureProvider,
        tempDirectory: File,
        generation: String,
        routeId: String,
        record: MmsProviderRecord,
        imported: Boolean,
        existing: NativeMmsAcquisition?,
    ): ProcessResult {
        if (record.messageType !in setOf(SEND_REQ, NOTIFICATION_IND, RETRIEVE_CONF)) {
            existing?.let { core.setState(it.acquisitionId, NativeMmsAcquisitionState.UNAVAILABLE, "provider_non_message_type") }
            return ProcessResult(true, unavailable = if (existing == null) 0 else 1)
        }
        val placeholder = input(generation, routeId, record, imported, body = inlineText(record))
        val acquisition = existing ?: try { core.begin(placeholder) } catch (_: Exception) { return ProcessResult(false) }
        when (acquisition.state) {
            NativeMmsAcquisitionState.COMPLETE -> return ProcessResult(true)
            NativeMmsAcquisitionState.UNAVAILABLE -> return ProcessResult(true, unavailable = 1)
            NativeMmsAcquisitionState.BLOCKED -> return ProcessResult(true, pending = 1)
            NativeMmsAcquisitionState.PENDING -> Unit
        }
        if (record.messageBox !in setOf(INBOX, SENT)) {
            core.setState(acquisition.acquisitionId, NativeMmsAcquisitionState.PENDING, "provider_box_pending")
            return ProcessResult(true, pending = 1)
        }
        if (!placeholder.incoming && placeholder.recipients.isEmpty() && record.bccRecipients.isNotEmpty()) {
            core.setState(acquisition.acquisitionId, NativeMmsAcquisitionState.BLOCKED, "bcc_only_recipients_not_modeled")
            return ProcessResult(true, pending = 1)
        }
        if (!record.ready) {
            val reason = record.error ?: "unsupported_metadata"
            val retryable = reason == "provider_content_pending"
            core.setState(acquisition.acquisitionId, if (retryable) NativeMmsAcquisitionState.PENDING else NativeMmsAcquisitionState.UNAVAILABLE, reason)
            return if (retryable) ProcessResult(true, pending = 1) else ProcessResult(true, unavailable = 1)
        }

        val fingerprint = fingerprint(record)
        if (acquisition.reason != "settling:$fingerprint") {
            core.setState(acquisition.acquisitionId, NativeMmsAcquisitionState.PENDING, "settling:$fingerprint")
            return ProcessResult(true, pending = 1, followup = true)
        }
        val stable = provider.read(record.id)
        if (stable == null) {
            core.setState(acquisition.acquisitionId, NativeMmsAcquisitionState.UNAVAILABLE, "provider_row_missing")
            return ProcessResult(true, unavailable = 1)
        }
        val stableFingerprint = fingerprint(stable)
        if (!stable.ready || stable.messageBox !in setOf(INBOX, SENT) || stableFingerprint != fingerprint) {
            core.setState(acquisition.acquisitionId, NativeMmsAcquisitionState.PENDING, "settling:$stableFingerprint")
            return ProcessResult(true, pending = 1, followup = true)
        }
        val body = try { readText(provider, stable) } catch (_: Exception) {
            core.setState(acquisition.acquisitionId, NativeMmsAcquisitionState.PENDING, "text_unavailable")
            return ProcessResult(true, pending = 1)
        }
        val updated = try { core.begin(input(generation, routeId, stable, imported, body)) } catch (_: Exception) {
            return ProcessResult(false)
        }
        return when (updated.state) {
            NativeMmsAcquisitionState.COMPLETE -> ProcessResult(true)
            NativeMmsAcquisitionState.UNAVAILABLE -> ProcessResult(true, unavailable = 1)
            NativeMmsAcquisitionState.BLOCKED -> ProcessResult(true, pending = 1)
            NativeMmsAcquisitionState.PENDING -> copyParts(core, provider, tempDirectory, updated, stable)
        }
    }

    private fun copyParts(core: MmsCaptureCore, provider: MmsCaptureProvider, tempDirectory: File, acquisition: NativeMmsAcquisition, record: MmsProviderRecord): ProcessResult {
        val existing = try { core.mappedParts(acquisition.acquisitionId) } catch (_: Exception) { return ProcessResult(true, pending = 1) }
        var binary = 0
        for (part in ordered(record.parts)) {
            if (part.mediaType.equals("text/plain", true)) continue
            if (++binary > MAX_BINARY) {
                core.setState(acquisition.acquisitionId, NativeMmsAcquisitionState.BLOCKED, "too_many_attachments")
                return ProcessResult(true, pending = 1)
            }
            if (existing.containsKey(part.providerPartId.toString())) continue
            val remaining = PENDING_MEDIA_LIMIT - core.pendingMediaBytes()
            if (remaining <= 0) {
                core.setState(acquisition.acquisitionId, NativeMmsAcquisitionState.PENDING, "media_quota")
                return ProcessResult(true, pending = 1)
            }
            val temp = File.createTempFile("mms-", ".part", tempDirectory)
            var attachmentId: String? = null
            try {
                provider.copyPart(part, temp, min(MAX_BYTES, remaining))
                attachmentId = core.prepareAttachment(temp.absolutePath, part.mediaType ?: "application/octet-stream", part.name ?: "part-${part.providerPartId}")
                core.mapPart(acquisition.acquisitionId, part.providerPartId.toString(), attachmentId)
            } catch (_: Exception) {
                if (attachmentId != null) try { core.discardAttachment(attachmentId) } catch (_: Exception) { }
                core.setState(acquisition.acquisitionId, NativeMmsAcquisitionState.PENDING, "part_unavailable")
                return ProcessResult(true, pending = 1)
            } finally { temp.delete() }
        }
        return try { core.complete(acquisition.acquisitionId); ProcessResult(true, captured = 1) }
        catch (_: MobileBindingsException.Conflict) {
            ProcessResult(true)
        }
        catch (_: MobileBindingsException.InvalidRequest) {
            core.setState(acquisition.acquisitionId, NativeMmsAcquisitionState.UNAVAILABLE, "invalid_mms_metadata")
            ProcessResult(true, unavailable = 1)
        }
        catch (_: Exception) {
            core.setState(acquisition.acquisitionId, NativeMmsAcquisitionState.PENDING, "parts_incomplete")
            ProcessResult(true, pending = 1)
        }
    }

    private fun input(generation: String, routeId: String, record: MmsProviderRecord, imported: Boolean, body: String): NativeMmsAcquisitionInput {
        val incoming = record.messageType == RETRIEVE_CONF || record.messageType == NOTIFICATION_IND || record.messageBox == INBOX
        val sender = record.sender.singleOrNull()?.takeIf { it.isNotBlank() }
        // BCC remains local provider provenance. Including it would disclose hidden recipients in group reply-all.
        val recipients = (record.toRecipients + record.ccRecipients).filter { it.isNotBlank() }.distinct()
        return NativeMmsAcquisitionInput(
            NativeMmsSource(generation, routeId, record.id.toString(), record.threadId?.toString()),
            incoming, sender, recipients, record.subject, body, imported,
            record.dateMs ?: System.currentTimeMillis(), record.transactionId,
        )
    }

    private fun inlineText(record: MmsProviderRecord) = ordered(record.parts)
        .filter { it.mediaType.equals("text/plain", true) }.joinToString("") { it.text.orEmpty() }

    private fun readText(provider: MmsCaptureProvider, record: MmsProviderRecord): String = buildString {
        for (part in ordered(record.parts).filter { it.mediaType.equals("text/plain", true) }) {
            append(provider.readPartText(part, MmsProviderReader.MAX_TEXT) ?: throw MmsProviderException("part text unavailable"))
            if (this.toString().toByteArray().size > MmsProviderReader.MAX_TEXT) throw MmsProviderException("message text exceeds budget")
        }
    }

    private fun ordered(parts: List<MmsProviderPart>) = parts.sortedWith(compareBy<MmsProviderPart> { it.sequence ?: Int.MAX_VALUE }.thenBy { it.providerPartId })

    private fun fingerprint(record: MmsProviderRecord): String {
        val digest = MessageDigest.getInstance("SHA-256")
        fun field(value: Any?) { val bytes = (value?.toString() ?: "<null>").toByteArray(); digest.update(bytes.size.toString().toByteArray()); digest.update(':'.code.toByte()); digest.update(bytes) }
        field(record.id); field(record.threadId); field(record.subscriptionId); field(record.messageBox); field(record.messageType)
        field(record.transactionId); field(record.subject); field(record.expiry); field(record.ready); field(record.error)
        record.sender.forEach(::field); field("to"); record.toRecipients.forEach(::field); field("cc"); record.ccRecipients.forEach(::field); field("bcc"); record.bccRecipients.forEach(::field)
        ordered(record.parts).forEach { field(it.providerPartId); field(it.sequence); field(it.mediaType); field(it.name); field(it.charset); field(it.text) }
        return digest.digest().joinToString("") { "%02x".format(it) }
    }

    private fun granted(context: Context) = ContextCompat.checkSelfPermission(context, Manifest.permission.READ_SMS) == PackageManager.PERMISSION_GRANTED
    private fun currentHighWater(context: Context): Long = context.contentResolver.query(Uri.parse("content://mms"), arrayOf("_id"), null, null, "_id DESC")?.use { if (it.moveToFirst()) it.getLong(0) else 0L } ?: 0L

    private const val INBOX = 1
    private const val SENT = 2
    private const val SEND_REQ = 128
    private const val NOTIFICATION_IND = 130
    private const val RETRIEVE_CONF = 132

    private data class ProcessResult(val durable: Boolean, val captured: Int = 0, val pending: Int = 0, val unavailable: Int = 0, val followup: Boolean = false)
    private data class ScanResult(val captured: Int, val pending: Int, val unavailable: Int, val more: Boolean, val finished: Boolean)
}

private class NativeCore(private val client: NativeClientInterface) : MmsCaptureCore {
    override fun acquisitions(limit: Int) = client.mmsAcquisitions(limit.toULong())
    override fun begin(input: NativeMmsAcquisitionInput) = client.beginMmsAcquisition(input)
    override fun setState(id: String, state: NativeMmsAcquisitionState, reason: String?) = client.setMmsAcquisitionState(id, state, reason)
    override fun checkpoint(generation: String, subscription: String, imported: Boolean) = client.mmsScanCheckpoint(generation, subscription, imported)
    override fun setCheckpoint(generation: String, subscription: String, imported: Boolean, providerId: String) = client.setMmsScanCheckpoint(generation, subscription, imported, providerId)
    override fun pendingMediaBytes() = client.mmsPendingMediaBytes().toLong()
    override fun prepareAttachment(path: String, mediaType: String, name: String) = client.prepareAttachment(path, mediaType, name).attachmentId
    override fun mapPart(id: String, providerPartId: String, attachmentId: String) = client.setMmsAcquisitionPart(id, providerPartId, attachmentId)
    override fun complete(id: String) { client.completeMmsAcquisition(id) }

    override fun mappedParts(id: String): Map<String, String> =
        client.mmsAcquisitionParts(id).associate { it.providerPartId to it.attachmentId }
    override fun discardAttachment(id: String) {
        client.discardUnreferencedAttachment(id)
    }
}
