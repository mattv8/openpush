package dev.openpush.mobile

import android.provider.ContactsContract.CommonDataKinds.Event
import android.provider.ContactsContract.CommonDataKinds.Nickname
import android.provider.ContactsContract.CommonDataKinds.Note
import android.provider.ContactsContract.CommonDataKinds.Organization
import android.provider.ContactsContract.CommonDataKinds.StructuredName
import dev.openpush.mobile.AndroidContactsProvider.Contact
import dev.openpush.mobile.AndroidContactsProvider.Failure
import dev.openpush.mobile.AndroidContactsProvider.Field
import dev.openpush.mobile.AndroidContactsProvider.FieldOp
import dev.openpush.mobile.AndroidContactsProvider.Kind
import dev.openpush.mobile.AndroidContactsProvider.Lookup
import dev.openpush.mobile.AndroidContactsProvider.Page
import dev.openpush.mobile.AndroidContactsProvider.PhotoOp
import dev.openpush.mobile.AndroidContactsProvider.PhotoOutcome
import dev.openpush.mobile.AndroidContactsProvider.PhotoRead
import dev.openpush.mobile.AndroidContactsProvider.WriteResult
import org.json.JSONArray
import org.json.JSONObject
import uniffi.openpush_mobile_bindings.MobileBindingsException
import java.io.File
import java.util.UUID

/** Runtime contacts permission state, read fresh at each decision point. */
internal data class ContactsAccess(val read: Boolean, val write: Boolean)

internal data class ContactPassReport(
    val outcome: Outcome,
    val bookId: String? = null,
    val captured: Int = 0,
    val observed: Int = 0,
    val scanFinished: Boolean = false,
    val scanComplete: Boolean = false,
    val deleted: Int = 0,
    val held: Int = 0,
    val applied: Int = 0,
    val failed: Int = 0,
    val outcomeUnknown: Int = 0,
    val awaitingApproval: Int = 0,
    /** Permits deferred until a referenced photo is downloaded and verified. */
    val waitingMedia: Int = 0,
    val more: Boolean = false,
) {
    enum class Outcome { DISABLED, RETIRED, NO_ACCESS, COMPLETED, PARTIAL }
}

internal data class ContactAccountChoice(val id: String, val name: String, val writable: Boolean)

/** One pending owner decision or recovery row, sanitized for settings (no provider IDs). */
internal data class ContactPendingItem(
    val id: String,
    val kind: String,
    val state: String,
    val displayName: String?,
    val fieldPaths: List<String>,
    val count: Int? = null,
    val isHold: Boolean = false,
)

internal data class ContactOverview(
    val bookId: String?,
    val state: String?,
    val remoteEdits: String?,
    val contactCount: Int?,
    val defaultAccountId: String?,
    val accounts: List<ContactAccountChoice>,
    val pending: List<ContactPendingItem>,
    val lastFullScanAt: Long?,
)

/**
 * One bounded contact pass on the already-open core: book state -> scan or incremental capture
 * -> owner edit permits. All durable state (book, policy, default account, IDs, revisions,
 * checkpoint, permits, pre-create evidence) is read from and written to the core.
 *
 * Never infers deletions outside a complete, full-access, authoritative scan; never retries an
 * issued or uncertain OS write; never writes without WRITE_CONTACTS (published capability).
 */
internal class ContactSyncCoordinator(
    private val core: ContactCore,
    private val os: ContactsOs,
    private val deviceId: String,
    private val access: () -> ContactsAccess,
    private val photoTempDirectory: File,
    /** Per-phone label for desktop book lists (e.g. `Build.MODEL`). */
    private val deviceName: String? = null,
    /** ISO 3166 region hint (SIM country, else locale) for phone-number resolution. */
    private val region: String? = null,
    private val clock: () -> Long = System::currentTimeMillis,
) {
    // ---- pass -----------------------------------------------------------------------------------

    fun run(charging: Boolean, forceFull: Boolean = false): ContactPassReport {
        val granted = access()
        val owned = ownedBook()
        val bookId = owned?.id ?: UUID.randomUUID().toString()
        val book = bookBody(bookId, owned?.generation ?: "1", granted, isNew = owned == null)
        captureBatch(book, emptyList())
        if (!granted.read) {
            // Permission loss publishes `unavailable` only; existing contacts are never removed.
            abandonScan(bookId, checkpoint(bookId))
            return ContactPassReport(ContactPassReport.Outcome.NO_ACCESS, bookId)
        }
        val defaultAccountId = defaultAccountId(bookId)
        val checkpoint = checkpoint(bookId)
        var report = if (forceFull || fullDue(checkpoint, charging)) {
            scanPass(book, bookId, checkpoint, defaultAccountId)
        } else {
            incrementalPass(book, bookId, checkpoint, defaultAccountId)
        }
        report = processRequests(book, bookId, defaultAccountId, report)
        return report
    }

    private fun fullDue(checkpoint: JSONObject, charging: Boolean): Boolean {
        if (checkpoint.has("scan_id") || checkpoint.optBoolean("full_requested")) return true
        val last = checkpoint.optLong("last_full_at", 0L).takeIf { it > 0 } ?: return true
        val age = clock() - last
        // Daily repair prefers charging but never waits forever for it.
        return (age >= FULL_REPAIR_MS && charging) || age >= FORCED_REPAIR_MS
    }

    private fun scanPass(book: JSONObject, bookId: String, checkpoint: JSONObject, defaultAccountId: String?): ContactPassReport {
        val scanId = checkpoint.optString("scan_id").ifEmpty { null } ?: UUID.randomUUID().toString().also { id ->
            core.beginScan(
                JSONObject().put("schema_version", 1).put("scan_id", id).put("book_id", bookId)
                    .put("generation", book.getString("generation")).put("access", "full").put("authoritative", true),
            )
            checkpoint.put("scan_id", id).put("scan_started_at", clock()).put("scan_incomplete", false).remove("scan_after")
            saveCheckpoint(bookId, checkpoint)
        }
        var budget = PASS_CONTACTS
        var photoBudget = PASS_PHOTOS
        var observed = 0
        while (budget > 0) {
            val after = if (checkpoint.has("scan_after")) checkpoint.getLong("scan_after") else null
            when (val page = os.scan(after, minOf(PAGE, budget), null)) {
                is Page.Failed -> {
                    // Cancelled by revocation or provider loss: terminal, never authoritative.
                    finish(scanId, complete = false)
                    clearScan(checkpoint)
                    saveCheckpoint(bookId, checkpoint)
                    if (page.reason == Failure.PERMISSION_DENIED) captureBatch(bookBody(bookId, book.getString("generation"), access(), false), emptyList())
                    return ContactPassReport(ContactPassReport.Outcome.PARTIAL, bookId, observed = observed, scanFinished = true)
                }
                is Page.Ready -> {
                    for (contact in page.contacts) {
                        val photo = photoFor(bookId, contact, photoBudget) ?: run {
                            // Photo work deferred: resume at this contact next pass.
                            saveCheckpoint(bookId, checkpoint)
                            return ContactPassReport(ContactPassReport.Outcome.PARTIAL, bookId, observed = observed, more = true)
                        }
                        if (photo.prepared) photoBudget--
                        val entry = ContactMapping.entry(contact, defaultAccountId, photo.field, photo.hash)
                        try {
                            core.observeScan(JSONObject().put("scan_id", scanId).put("source", entry))
                        } catch (_: MobileBindingsException.InvalidRequest) {
                            // A contact the core cannot accept is unobserved: the scan cannot be authoritative.
                            checkpoint.put("scan_incomplete", true)
                        }
                        checkpoint.put("scan_after", contact.contactId)
                        observed++
                        budget--
                    }
                    page.nextAfterId?.let { checkpoint.put("scan_after", it) }
                    if (page.incomplete) checkpoint.put("scan_incomplete", true)
                    saveCheckpoint(bookId, checkpoint)
                    if (!page.hasMore) {
                        val now = access()
                        val complete = !checkpoint.optBoolean("scan_incomplete") && now.read
                        val finished = finish(scanId, complete)
                        if (complete) {
                            val startedAt = checkpoint.optLong("scan_started_at", clock())
                            checkpoint.put("last_full_at", startedAt).put("updated_since", startedAt - SKEW_MS)
                                .put("deleted_since", startedAt - SKEW_MS).remove("full_requested")
                        }
                        clearScan(checkpoint)
                        saveCheckpoint(bookId, checkpoint)
                        return ContactPassReport(
                            if (complete) ContactPassReport.Outcome.COMPLETED else ContactPassReport.Outcome.PARTIAL, bookId,
                            observed = observed, scanFinished = true, scanComplete = complete,
                            deleted = finished.optInt("deleted"), held = finished.optInt("held"),
                        )
                    }
                }
            }
        }
        return ContactPassReport(ContactPassReport.Outcome.PARTIAL, bookId, observed = observed, more = true)
    }

    private fun incrementalPass(book: JSONObject, bookId: String, checkpoint: JSONObject, defaultAccountId: String?): ContactPassReport {
        val since = if (checkpoint.has("inc_since")) checkpoint.getLong("inc_since") else checkpoint.getLong("updated_since")
        if (!checkpoint.has("inc_since")) checkpoint.put("inc_since", since).put("inc_started_at", clock()).remove("inc_after")
        var budget = PASS_CONTACTS
        var photoBudget = PASS_PHOTOS
        var captured = 0
        while (budget > 0) {
            val after = if (checkpoint.has("inc_after")) checkpoint.getLong("inc_after") else null
            when (val page = os.scan(after, minOf(PAGE, budget), since)) {
                is Page.Failed -> {
                    saveCheckpoint(bookId, checkpoint)
                    return ContactPassReport(ContactPassReport.Outcome.PARTIAL, bookId, captured = captured)
                }
                is Page.Ready -> {
                    val entries = mutableListOf<JSONObject>()
                    var deferred = false
                    for (contact in page.contacts) {
                        val photo = photoFor(bookId, contact, photoBudget)
                        if (photo == null) { deferred = true; break }
                        if (photo.prepared) photoBudget--
                        entries += ContactMapping.entry(contact, defaultAccountId, photo.field, photo.hash)
                        budget--
                    }
                    if (entries.isNotEmpty()) {
                        captureTolerant(book, entries)
                        captured += entries.size
                        checkpoint.put("inc_after", page.contacts[entries.size - 1].contactId)
                    }
                    if (deferred) {
                        saveCheckpoint(bookId, checkpoint)
                        return ContactPassReport(ContactPassReport.Outcome.PARTIAL, bookId, captured = captured, more = true)
                    }
                    page.nextAfterId?.let { checkpoint.put("inc_after", it) }
                    if (!page.hasMore) {
                        val startedAt = checkpoint.optLong("inc_started_at", clock())
                        // Deletions are only ever inferred by a full scan; history merely schedules one.
                        val deletedSince = checkpoint.optLong("deleted_since", since)
                        when (val deleted = os.deletedSince(deletedSince)) {
                            null -> Unit
                            else -> {
                                if (deleted > 0) checkpoint.put("full_requested", true)
                                checkpoint.put("deleted_since", startedAt - SKEW_MS)
                            }
                        }
                        checkpoint.put("updated_since", startedAt - SKEW_MS).remove("inc_since")
                        checkpoint.remove("inc_after")
                        checkpoint.remove("inc_started_at")
                        saveCheckpoint(bookId, checkpoint)
                        return ContactPassReport(ContactPassReport.Outcome.COMPLETED, bookId, captured = captured, more = checkpoint.optBoolean("full_requested"))
                    }
                    saveCheckpoint(bookId, checkpoint)
                }
            }
        }
        return ContactPassReport(ContactPassReport.Outcome.PARTIAL, bookId, captured = captured, more = true)
    }

    private fun finish(scanId: String, complete: Boolean): JSONObject =
        core.finishScan(JSONObject().put("schema_version", 1).put("scan_id", scanId).put("complete", complete))

    private fun abandonScan(bookId: String, checkpoint: JSONObject) {
        val scanId = checkpoint.optString("scan_id").ifEmpty { return }
        finish(scanId, complete = false)
        clearScan(checkpoint)
        saveCheckpoint(bookId, checkpoint)
    }

    private fun clearScan(checkpoint: JSONObject) {
        for (key in listOf("scan_id", "scan_after", "scan_incomplete", "scan_started_at")) checkpoint.remove(key)
    }

    private fun checkpoint(bookId: String): JSONObject =
        core.scanState(JSONObject().put("schema_version", 1).put("book_id", bookId)).optJSONObject("checkpoint") ?: JSONObject()

    private fun saveCheckpoint(bookId: String, checkpoint: JSONObject) {
        core.scanState(JSONObject().put("schema_version", 1).put("book_id", bookId).put("checkpoint", checkpoint))
    }

    /** Captures a batch; if the core rejects it, captures entries singly so one bad row cannot wedge capture. */
    private fun captureTolerant(book: JSONObject, entries: List<JSONObject>) {
        try {
            captureBatch(book, entries)
        } catch (_: MobileBindingsException.InvalidRequest) {
            for (entry in entries) {
                try { captureBatch(book, listOf(entry)) } catch (_: MobileBindingsException.InvalidRequest) { }
            }
        }
    }

    private fun captureBatch(book: JSONObject, entries: List<JSONObject>): JSONObject {
        require(entries.size <= BATCH)
        return core.capture(JSONObject().put("schema_version", 1).put("book", book).put("contacts", JSONArray(entries)))
    }

    // ---- photos ---------------------------------------------------------------------------------

    private class PhotoChoice(val field: ContactMapping.PhotoField, val hash: String?, val prepared: Boolean)

    /**
     * Lazy photo capture (core semantics): text is captured with `photo` omitted, so the stored
     * photo is inherited; only when the provider fingerprint differs from the
     * `photo_source_hash` core recorded is the photo read, normalized by core and set (with the
     * new hash). Null means preparation would exceed this pass's budget (caller defers).
     * An unreadable photo is inherited, never published as a removal.
     */
    private fun photoFor(bookId: String, contact: Contact, budget: Int): PhotoChoice? {
        val fingerprint = photoFingerprint(contact) ?: return PhotoChoice(ContactMapping.PhotoField.Clear, null, false)
        val stored = core.sourceContext(JSONObject().put("schema_version", 1).put("book_id", bookId).put("source_key", ContactMapping.sourceKey(contact)))
            ?.optJSONObject("provenance")?.optString("photo_source_hash")?.ifEmpty { null }
        if (stored == fingerprint) return PhotoChoice(ContactMapping.PhotoField.Inherit, null, false)
        if (budget <= 0) return null
        return when (val read = os.readPhoto(contact.contactId)) {
            is PhotoRead.Bytes -> {
                photoTempDirectory.mkdirs()
                val temp = File.createTempFile("contact-photo-", ".img", photoTempDirectory)
                try {
                    temp.writeBytes(read.bytes)
                    val id = try {
                        core.preparePhoto(temp.path)
                    } catch (_: MobileBindingsException.InvalidRequest) {
                        null // unsupported, animated or oversized input
                    }
                    // A rejected photo is recorded by fingerprint so it is not retried every pass.
                    PhotoChoice(id?.let { ContactMapping.PhotoField.Set(it) } ?: ContactMapping.PhotoField.Clear, fingerprint, true)
                } finally {
                    temp.delete()
                }
            }
            PhotoRead.TooLarge -> PhotoChoice(ContactMapping.PhotoField.Clear, fingerprint, false)
            PhotoRead.None -> PhotoChoice(ContactMapping.PhotoField.Clear, null, false)
            PhotoRead.Failed -> PhotoChoice(ContactMapping.PhotoField.Inherit, null, false)
        }
    }

    /** Provider identity of the aggregate's chosen photo row, or null when it has none. */
    private fun photoFingerprint(contact: Contact): String? {
        val row = contact.photoDataId?.let { id -> contact.fields.firstOrNull { it.kind == Kind.PHOTO && it.dataId == id } } ?: return null
        return "${row.dataId}:${row.version}:${row.values[android.provider.ContactsContract.CommonDataKinds.Photo.PHOTO_FILE_ID].orEmpty()}"
    }

    // ---- owner edit permits ---------------------------------------------------------------------

    private fun processRequests(book: JSONObject, bookId: String, defaultAccountId: String?, report: ContactPassReport): ContactPassReport {
        var applied = 0
        var failed = 0
        var unknown = 0
        var awaiting = 0
        var waitingMedia = 0
        var handled = 0
        val rows = core.listRequests(JSONObject().put("book_id", bookId)).optJSONArray("requests") ?: JSONArray()
        for (index in 0 until rows.length()) {
            if (handled >= PASS_REQUESTS) {
                return report.copy(applied = applied, failed = failed, outcomeUnknown = unknown, awaitingApproval = awaiting, waitingMedia = waitingMedia, more = true)
            }
            val row = rows.getJSONObject(index)
            if (row.optString("kind") == "scan_deletions") continue
            val id = row.optString("request_id").takeIf { it.isNotEmpty() } ?: continue
            val state = row.optString("state")
            if (state !in OPEN_STATES) continue
            handled++
            val permit = core.nextPermit(JSONObject().put("schema_version", 1).put("request_id", id))
            when (permit.optString("status")) {
                "permit" -> when (val outcome = apply(permit, bookId, defaultAccountId).also { if (it is Outcome.Failed && it.reason == STALE) requestFullScan(bookId) }) {
                    is Outcome.Applied -> { reconcileApplied(id, outcome.observed); applied++ }
                    is Outcome.Failed -> { reconcileFailed(id, outcome.reason, outcome.observed, book); failed++ }
                    Outcome.Unknown -> { reconcile(JSONObject().put("outcome", "unknown"), id); unknown++ }
                }
                "outcome_unknown" -> when (val outcome = recover(permit, defaultAccountId)) {
                    is Outcome.Applied -> { reconcileApplied(id, outcome.observed); applied++ }
                    else -> unknown++
                }
                "awaiting_approval" -> awaiting++
                // Core issued no permit: the photo downloads in this pass's media phase; ask again later.
                "waiting_media" -> waitingMedia++
            }
        }
        return report.copy(
            applied = applied, failed = failed, outcomeUnknown = unknown, awaitingApproval = awaiting,
            waitingMedia = waitingMedia, more = report.more || waitingMedia > 0,
        )
    }

    private fun paths(row: JSONObject): List<String> {
        val array = row.optJSONArray("field_paths") ?: return emptyList()
        return (0 until array.length()).map { array.getString(it) }
    }

    private sealed interface Outcome {
        /** [observed] is a platform source entry (or null for a delete). */
        class Applied(val observed: JSONObject?) : Outcome
        /** No OS effect, or a partial effect whose actual state is [observed]. */
        class Failed(val reason: String, val observed: JSONObject? = null) : Outcome
        data object Unknown : Outcome
    }

    private fun reconcile(input: JSONObject, requestId: String) {
        core.reconcile(input.put("schema_version", 1).put("request_id", requestId))
    }

    private fun reconcileApplied(requestId: String, observed: JSONObject?) {
        val input = JSONObject().put("outcome", "applied")
        if (observed != null) input.put("observed_source", observed)
        reconcile(input, requestId)
    }

    private fun reconcileFailed(requestId: String, reason: String, observed: JSONObject?, book: JSONObject) {
        // A partial OS effect or a stale refusal still publishes the observed owner state.
        if (observed != null) captureTolerant(book, listOf(observed))
        reconcile(JSONObject().put("outcome", "failed").put("reason", reason), requestId)
    }

    private fun apply(permit: JSONObject, bookId: String, defaultAccountId: String?): Outcome {
        if (!access().write) return Outcome.Failed("permission_denied")
        val request = permit.getJSONObject("request")
        return when (request.optString("kind")) {
            "create" -> applyCreate(permit, request, defaultAccountId)
            "update" -> applyUpdate(permit, request, defaultAccountId)
            "delete" -> applyDelete(permit, defaultAccountId)
            else -> Outcome.Failed("invalid_request")
        }
    }

    private fun requestFullScan(bookId: String) {
        val checkpoint = checkpoint(bookId)
        if (!checkpoint.optBoolean("full_requested")) saveCheckpoint(bookId, checkpoint.put("full_requested", true))
    }

    /** Fresh OS state of a captured core contact, resolved by its captured raw contacts only. */
    private sealed interface Resolved {
        /** The aggregate holds exactly the captured raw set. */
        class Match(val contact: Contact) : Resolved
        /** Joined, split or partially deleted since capture; [fresh] is one current aggregate. */
        class Changed(val fresh: Contact?) : Resolved
        /** None of the captured raws exists any more. */
        data object Gone : Resolved
        class Failed(val reason: String) : Resolved
    }

    private fun capturedRaws(permit: JSONObject): Set<Long>? {
        val ids = permit.optJSONObject("source")?.optJSONObject("provenance")?.optJSONArray("raw_ids") ?: return null
        return (0 until ids.length()).mapNotNull { ids.optString(it).toLongOrNull() }.toSet().takeIf { it.size == ids.length() && it.isNotEmpty() }
    }

    private fun resolve(captured: Set<Long>): Resolved {
        var changed: Contact? = null
        for (raw in captured.sorted()) {
            when (val lookup = os.contactByRawContact(raw)) {
                is Lookup.Found -> {
                    if (ContactMapping.rawIds(lookup.contact) == captured) return Resolved.Match(lookup.contact)
                    if (changed == null) changed = lookup.contact
                }
                Lookup.Missing -> Unit
                is Lookup.Failed -> return Resolved.Failed(if (lookup.reason == Failure.PERMISSION_DENIED) "permission_denied" else "provider_unavailable")
            }
        }
        if (changed != null) return Resolved.Changed(changed)
        // Some captured raws missing and none found means all are gone.
        return Resolved.Gone
    }

    /**
     * The OS no longer matches what the request was based on: publish the fresh observed state
     * (under its own raw identity), refuse with `stale_item` and schedule a full scan. No write.
     */
    private fun stale(fresh: Contact?, defaultAccountId: String?): Outcome.Failed =
        Outcome.Failed(STALE, fresh?.let { observedEntry(it, defaultAccountId, null, null) })

    /** Whether the fresh OS contact, in core terms, still equals `permit.current` on [roots]. */
    private fun unchangedSinceBase(permit: JSONObject, fresh: Contact, roots: Collection<String>): Boolean {
        val current = permit.optJSONObject("current") ?: return false
        val fieldSources = permit.optJSONObject("source")?.optJSONArray("field_sources") ?: JSONArray()
        val view = ContactMapping.coreView(fresh, fieldSources)
        return roots.all { root -> ContactMapping.same(view.opt(root), current.opt(root)) }
    }

    private fun photoUnchangedSinceBase(permit: JSONObject, fresh: Contact): Boolean {
        val captured = permit.optJSONObject("source")?.optJSONObject("provenance")
            ?.opt("photo_source_hash") as? String
        return captured == photoFingerprint(fresh)
    }

    private fun patchedRoots(request: JSONObject): Set<String> {
        val patches = request.optJSONArray("patches") ?: return emptySet()
        return (0 until patches.length()).mapTo(HashSet()) { index ->
            patches.getJSONObject(index).optString("path").split('.', '[').first()
        }
    }

    private fun requestPhotoId(attachment: JSONObject?): String? =
        attachment?.let { it.optJSONObject("attachment_id")?.optString("attachment_id") ?: it.optString("attachment_id") }?.takeIf { it.isNotEmpty() }

    private fun photoBytes(attachment: JSONObject?): ByteArray? {
        val id = requestPhotoId(attachment) ?: return null
        return core.attachmentBytes(id, AndroidContactsProvider.MAX_NORMALIZED_PHOTO_BYTES)
    }

    private fun createAccount(permit: JSONObject, defaultAccountId: String?): Pair<Boolean, ContactAccount?> {
        val wanted = permit.optJSONObject("book_source")?.optString("default_account_id")?.ifEmpty { null } ?: defaultAccountId
        if (wanted == null || wanted == ContactMapping.LOCAL_ACCOUNT_ID) return true to null
        val account = os.accounts().firstOrNull { ContactMapping.accountId(it) == wanted } ?: return false to null
        return true to account
    }

    private fun applyCreate(permit: JSONObject, request: JSONObject, defaultAccountId: String?): Outcome {
        val (known, account) = createAccount(permit, defaultAccountId)
        if (!known) return Outcome.Failed("read_only_account")
        val fields = ContactMapping.newFields(request)
        if (fields.isEmpty()) return Outcome.Failed("invalid_request")
        val photo = if (request.has("photo") && !request.isNull("photo")) {
            photoBytes(request.optJSONObject("photo")) ?: return Outcome.Failed("photo_unavailable")
        } else null
        // Durable, write-once evidence of pre-existing matches BEFORE the OS create.
        val before = os.findCreateMatches(account, fields) ?: return Outcome.Failed("unattributable_create")
        if (before.size > MAX_EVIDENCE) return Outcome.Failed("ambiguous_existing_contacts")
        val requestId = permit.getString("request_id")
        core.applyEvidence(JSONObject().put("schema_version", 1).put("request_id", requestId).put("before_source_ids", JSONArray(before.map(Long::toString))))
        return when (val result = os.create(account, fields, photo)) {
            is WriteResult.Applied -> {
                val written = if (result.photo == PhotoOutcome.WRITTEN) requestPhotoId(request.optJSONObject("photo")) else null
                if (result.photo == PhotoOutcome.UNKNOWN) Outcome.Unknown else observedRaw(result.rawContactId, defaultAccountId, written) ?: Outcome.Unknown
            }
            WriteResult.OutcomeUnknown -> Outcome.Unknown
            WriteResult.ReadOnly -> Outcome.Failed("read_only_account")
            WriteResult.PermissionDenied -> Outcome.Failed("permission_denied")
            is WriteResult.Invalid -> Outcome.Failed("unsupported_field")
            WriteResult.Conflict, WriteResult.NotFound -> Outcome.Failed("platform_failed")
        }
    }

    /**
     * Post-create evidence for the created raw contact. If the provider auto-joined it with
     * pre-existing raws, only the created raw's part is observed (its own raw identity), so the
     * request never binds to contacts it did not create.
     */
    private fun observedRaw(rawContactId: Long, defaultAccountId: String?, writtenPhoto: String? = null): Outcome.Applied? =
        when (val lookup = os.contactByRawContact(rawContactId)) {
            is Lookup.Found -> {
                val contact = lookup.contact
                val scoped = if (ContactMapping.rawIds(contact) == setOf(rawContactId)) contact else ContactMapping.rawScoped(contact, rawContactId)
                Outcome.Applied(observedEntry(scoped, defaultAccountId, writtenPhoto, rawContactId))
            }
            else -> null
        }

    /** Post-write evidence for an update, re-resolved by the same captured raw set. */
    private fun observedCaptured(captured: Set<Long>, defaultAccountId: String?, writtenPhoto: String?, writtenRaw: Long?): JSONObject? =
        (resolve(captured) as? Resolved.Match)?.let { observedEntry(it.contact, defaultAccountId, writtenPhoto, writtenRaw) }

    /**
     * Post-write evidence entry. A photo this write installed is referenced by its attachment
     * only when the photo the aggregate actually displays comes from the raw it was written to;
     * otherwise the visible photo is re-captured, so the core never records an unseen photo.
     */
    private fun observedEntry(contact: Contact, defaultAccountId: String?, writtenPhoto: String?, writtenRaw: Long?): JSONObject {
        val book = currentBookId
        val fingerprint = photoFingerprint(contact)
        val shownRaw = contact.photoDataId?.let { id -> contact.fields.firstOrNull { it.dataId == id }?.rawContactId }
        val photo = when {
            fingerprint != null && writtenPhoto != null && writtenRaw != null && shownRaw == writtenRaw ->
                PhotoChoice(ContactMapping.PhotoField.Set(writtenPhoto), fingerprint, false)
            contact.lookupKey.isEmpty() -> null // raw-scoped view: the aggregate photo is not attributable
            book != null -> photoFor(book, contact, 1)
            else -> null
        } ?: PhotoChoice(ContactMapping.PhotoField.Inherit, null, false)
        return ContactMapping.entry(contact, defaultAccountId, photo.field, photo.hash)
    }

    private var currentBookId: String? = null

    private fun applyUpdate(permit: JSONObject, request: JSONObject, defaultAccountId: String?): Outcome {
        val captured = capturedRaws(permit) ?: return Outcome.Failed(STALE) // pre-raw-identity mapping
        val contact = when (val resolved = resolve(captured)) {
            is Resolved.Match -> resolved.contact
            is Resolved.Changed -> return stale(resolved.fresh, defaultAccountId)
            Resolved.Gone -> return Outcome.Failed("not_found")
            is Resolved.Failed -> return Outcome.Failed(resolved.reason)
        }
        // The request was based on `permit.current`; a phone-side change since then wins.
        if (!unchangedSinceBase(permit, contact, patchedRoots(request))) return stale(contact, defaultAccountId)
        if (request.optJSONObject("photo_op") != null && !photoUnchangedSinceBase(permit, contact)) {
            return stale(contact, defaultAccountId)
        }
        val fieldSources = permit.optJSONObject("source")?.optJSONArray("field_sources") ?: JSONArray()
        val plan = when (val planned = UpdatePlanner(contact, defaultAccountId, fieldSources).plan(request)) {
            is UpdatePlanner.Result.Rejected -> return Outcome.Failed(planned.reason)
            is UpdatePlanner.Result.Planned -> planned
        }
        val photoOp = request.optJSONObject("photo_op")
        var photoRaw: Long? = null
        if (photoOp?.optString("op") == "set") {
            val bytes = photoBytes(photoOp) ?: return Outcome.Failed("photo_unavailable")
            val raw = plan.photoRaw ?: return Outcome.Failed("read_only_account")
            plan.photos[raw] = PhotoOp.Write(bytes)
            photoRaw = raw
        }
        // Only the captured raws (== the fresh aggregate's raws) are ever written.
        if (!captured.containsAll(plan.ops.keys + plan.photos.keys)) return stale(contact, defaultAccountId)
        var appliedAny = false
        var photoUnknown = false
        for (raw in (plan.ops.keys + plan.photos.keys).distinct()) {
            when (val result = os.update(raw, plan.ops[raw].orEmpty(), plan.photos[raw])) {
                is WriteResult.Applied -> { appliedAny = true; if (result.photo == PhotoOutcome.UNKNOWN) photoUnknown = true }
                WriteResult.OutcomeUnknown -> return Outcome.Unknown
                else -> {
                    val reason = when (result) {
                        WriteResult.ReadOnly -> "read_only_account"
                        WriteResult.PermissionDenied -> "permission_denied"
                        WriteResult.Conflict, WriteResult.NotFound -> STALE
                        is WriteResult.Invalid -> "unsupported_field"
                        else -> "platform_failed"
                    }
                    if (!appliedAny) return Outcome.Failed(reason)
                    // Earlier raw contacts changed: report the real OS state, never a phantom success.
                    return Outcome.Failed("partially_applied", observedCaptured(captured, defaultAccountId, null, null))
                }
            }
        }
        if (photoUnknown) return Outcome.Unknown
        val written = if (photoOp?.optString("op") == "set") requestPhotoId(photoOp) else null
        return observedCaptured(captured, defaultAccountId, written, photoRaw)?.let { Outcome.Applied(it) } ?: Outcome.Unknown
    }

    private fun applyDelete(permit: JSONObject, defaultAccountId: String?): Outcome {
        // Without a captured raw identity there is nothing safe to delete and no evidence of absence.
        val captured = capturedRaws(permit) ?: return Outcome.Failed(if (permit.optJSONObject("source") == null) "not_found" else STALE)
        val contact = when (val resolved = resolve(captured)) {
            is Resolved.Match -> resolved.contact
            is Resolved.Changed -> return stale(resolved.fresh, defaultAccountId)
            Resolved.Gone -> return Outcome.Applied(null) // every captured raw is already gone
            is Resolved.Failed -> return Outcome.Failed(resolved.reason)
        }
        if (!unchangedSinceBase(permit, contact, ContactMapping.ROOTS)) return stale(contact, defaultAccountId)
        if (!photoUnchangedSinceBase(permit, contact)) return stale(contact, defaultAccountId)
        // Raw contacts of read-only accounts cannot be removed; never delete part of a contact.
        if (contact.sources.any { !it.writable }) return Outcome.Failed("read_only_account")
        var deletedAny = false
        for (raw in contact.sources) {
            when (os.delete(raw.rawContactId, raw.version)) {
                is WriteResult.Applied, WriteResult.NotFound -> deletedAny = true
                WriteResult.PermissionDenied -> return if (deletedAny) Outcome.Unknown else Outcome.Failed("permission_denied")
                WriteResult.Conflict -> return if (deletedAny) Outcome.Unknown else Outcome.Failed(STALE)
                else -> return Outcome.Unknown
            }
        }
        return if (resolve(captured) == Resolved.Gone) Outcome.Applied(null) else Outcome.Unknown
    }

    /** Recovery of an `outcome_unknown` permit from OS evidence only; never re-issues the write. */
    private fun recover(permit: JSONObject, defaultAccountId: String?): Outcome {
        val request = permit.optJSONObject("request") ?: return Outcome.Unknown
        if (!access().read) return Outcome.Unknown
        return when (request.optString("kind")) {
            "delete" -> {
                val captured = capturedRaws(permit) ?: return Outcome.Unknown
                if (resolve(captured) == Resolved.Gone) Outcome.Applied(null) else Outcome.Unknown
            }
            "create" -> {
                val evidence = core.applyEvidence(JSONObject().put("schema_version", 1).put("request_id", permit.getString("request_id")))
                    .optJSONObject("evidence") ?: return Outcome.Unknown
                val before = evidence.optJSONArray("before_source_ids")?.let { ids -> (0 until ids.length()).map { ids.getString(it).toLong() }.toSet() }
                    ?: return Outcome.Unknown
                val (known, account) = createAccount(permit, defaultAccountId)
                if (!known) return Outcome.Unknown
                val after = os.findCreateMatches(account, ContactMapping.newFields(request)) ?: return Outcome.Unknown
                // Adopt only a uniquely attributable new contact; anything else needs a local decision.
                val created = (after - before).singleOrNull() ?: return Outcome.Unknown
                observedRaw(created, defaultAccountId) ?: Outcome.Unknown
            }
            else -> Outcome.Unknown
        }
    }

    // ---- book -------------------------------------------------------------------------------------

    data class OwnedBook(val id: String, val generation: String, val state: String)

    fun ownedBook(): OwnedBook? {
        val books = core.listBooks().optJSONArray("books") ?: return null
        for (index in 0 until books.length()) {
            val book = books.getJSONObject(index)
            if (book.optString("owner_device_id") != deviceId || book.optString("state") == "retired") continue
            val id = book.optString("id").takeIf { it.isNotEmpty() } ?: continue
            currentBookId = id
            return OwnedBook(id, book.optString("generation").ifEmpty { "1" }, book.optString("state"))
        }
        return null
    }

    private fun bookBody(id: String, generation: String, granted: ContactsAccess, isNew: Boolean, state: String? = null): JSONObject {
        val accounts = JSONArray().put(JSONObject().put("id", ContactMapping.LOCAL_ACCOUNT_ID).put("name", ContactMapping.accountName(null)).put("writable", granted.write))
        val listed = if (granted.read) runCatching { os.accounts() }.getOrDefault(emptyList()) else emptyList()
        for (account in listed) {
            accounts.put(JSONObject().put("id", ContactMapping.accountId(account)).put("name", ContactMapping.accountName(account)).put("writable", granted.write))
        }
        val book = JSONObject()
            .put("id", id).put("owner_device_id", deviceId).put("generation", generation)
            .put("state", state ?: if (granted.read) "active" else "unavailable")
            .put("capabilities", JSONObject().put("read", granted.read).put("write", granted.read && granted.write)
                .put("photo", true).put("notes", true).put("birthday", true))
            .put("accounts", accounts)
        deviceName?.takeIf { it.isNotBlank() }?.let { book.put("device_name", it.take(128)) }
        region?.takeIf { it.length == 2 && it.all(Char::isLetter) }?.let { book.put("region", it.uppercase()) }
        // A suggestion only: core keeps the owner's stored default while it is still listed.
        val osDefault = (if (granted.read) os.osDefaultAccount() else null) as? AndroidContactsProvider.DefaultAccount.Cloud
        book.put("default_account_id", osDefault?.account?.takeIf { it in listed }?.let(ContactMapping::accountId) ?: ContactMapping.LOCAL_ACCOUNT_ID)
        if (isNew) book.put("policy", JSONObject().put("remote_edits", "auto").put("large_delete_requires_approval", true))
        currentBookId = id
        return book
    }

    private fun defaultAccountId(bookId: String): String? =
        core.settings(JSONObject().put("schema_version", 1).put("book_id", bookId)).optString("default_account_id").ifEmpty { null }

    // ---- settings (core-only; never an OS write) ---------------------------------------------------

    fun overview(): ContactOverview {
        val book = core.listBooks().optJSONArray("books")?.let { books ->
            (0 until books.length()).map { books.getJSONObject(it) }
                .firstOrNull { it.optString("owner_device_id") == deviceId && it.optString("state") != "retired" }
        } ?: return ContactOverview(null, null, null, null, null, emptyList(), emptyList(), null)
        val bookId = book.getString("id")
        val settings = core.settings(JSONObject().put("schema_version", 1).put("book_id", bookId))
        val accounts = settings.optJSONArray("accounts")?.let { array ->
            (0 until array.length()).map { array.getJSONObject(it) }.mapNotNull { account ->
                account.optString("id").ifEmpty { null }?.let { ContactAccountChoice(it, account.optString("name"), account.optBoolean("writable", true)) }
            }
        }.orEmpty()
        val rows = core.listRequests(JSONObject().put("book_id", bookId)).optJSONArray("requests") ?: JSONArray()
        val pending = (0 until rows.length()).map { rows.getJSONObject(it) }.mapNotNull { row ->
            if (row.optString("kind") == "scan_deletions") {
                ContactPendingItem(row.getString("scan_id"), "scan_deletions", row.optString("state"), null, emptyList(), row.optInt("count"), isHold = true)
            } else {
                val state = row.optString("state")
                if (state !in OPEN_STATES && state != "awaiting_approval") null
                else ContactPendingItem(row.getString("request_id"), row.optString("kind"), state, row.optString("display_name").ifEmpty { null }, paths(row))
            }
        }
        val checkpoint = checkpoint(bookId)
        return ContactOverview(
            bookId, book.optString("state"), book.optString("effective_remote_edits").ifEmpty { book.optJSONObject("policy")?.optString("remote_edits") },
            book.optInt("contact_count"), settings.optString("default_account_id").ifEmpty { null }, accounts, pending,
            checkpoint.optLong("last_full_at", 0L).takeIf { it > 0 },
        )
    }

    fun setRemoteEdits(mode: String) {
        require(mode in setOf("auto", "confirm", "off"))
        val book = ownedBook() ?: throw IllegalStateException("no owned contact book")
        core.settings(JSONObject().put("schema_version", 1).put("book_id", book.id).put("policy", JSONObject().put("remote_edits", mode)))
    }

    fun setDefaultAccount(accountId: String) {
        val book = ownedBook() ?: throw IllegalStateException("no owned contact book")
        core.settings(JSONObject().put("schema_version", 1).put("book_id", book.id).put("default_account_id", accountId))
    }

    /** Owner decision for an edit request or a held scan deletion. Never an OS write. */
    fun decide(item: ContactPendingItem, approve: Boolean) {
        val key = if (item.isHold) "scan_id" else "request_id"
        core.approval(JSONObject().put(key, item.id).put("approve", approve))
    }

    /**
     * Local resolution of an uncertain write the phone cannot attribute: the requester is told it
     * failed. The OS keeps whatever it has; the next scan publishes the actual state.
     */
    fun resolveUnknown(requestId: String) {
        reconcile(JSONObject().put("outcome", "failed").put("reason", "owner_resolved"), requestId)
    }

    /** Publishes the owned book as retired and stops using it. Never deletes OS contacts. */
    fun retire(): Boolean {
        val book = ownedBook() ?: return false
        val body = bookBody(book.id, book.generation, ContactsAccess(read = false, write = false), isNew = false, state = "retired")
        captureBatch(body, emptyList())
        return true
    }

    companion object {
        const val BATCH = 200
        const val PAGE = 200
        const val PASS_CONTACTS = 600
        const val PASS_PHOTOS = 40
        const val PASS_REQUESTS = 10
        const val MAX_EVIDENCE = 200
        const val SKEW_MS = 5_000L
        const val FULL_REPAIR_MS = 24L * 60 * 60 * 1000
        const val FORCED_REPAIR_MS = 36L * 60 * 60 * 1000
        const val STALE = "stale_item"
        val OPEN_STATES = setOf("requested", "approved", "applying", "outcome_unknown")
    }
}

/**
 * Translates validated core patches into provider operations against the freshly read contact.
 * Core item IDs map to provider `Data._ID`s through the owner's field sources; scalar changes to
 * the same row merge into one version-guarded replace. Read-only rows are refused before writes.
 */
internal class UpdatePlanner(
    private val contact: Contact,
    defaultAccountId: String?,
    fieldSources: JSONArray,
) {
    sealed interface Result {
        class Planned(val ops: Map<Long, List<FieldOp>>, val photos: MutableMap<Long, PhotoOp>, val photoRaw: Long?) : Result
        class Rejected(val reason: String) : Result
    }

    private class Rejection(val reason: String) : Exception()

    private val target = ContactMapping.targetRaw(contact, defaultAccountId)
    private val toNative: Map<Pair<String, String>, String> = (0 until fieldSources.length()).associate { index ->
        val source = fieldSources.getJSONObject(index)
        (source.optString("field") to source.optString("id")) to source.optString("source_id")
    }
    private val rowValues = LinkedHashMap<Long, MutableMap<String, String?>>()
    private val removes = LinkedHashSet<Long>()
    private val adds = LinkedHashMap<Long, MutableList<AndroidContactsProvider.NewField>>()
    private val scalarAdds = LinkedHashMap<Kind, MutableMap<String, String?>>()
    private val photos = LinkedHashMap<Long, PhotoOp>()

    fun plan(request: JSONObject): Result = try {
        val patches = request.optJSONArray("patches") ?: JSONArray()
        val namePatched = (0 until patches.length()).any { patches.getJSONObject(it).optString("path").startsWith("name.") }
        for (index in 0 until patches.length()) apply(patches.getJSONObject(index), namePatched)
        if (request.optJSONObject("photo_op")?.optString("op") == "remove") {
            for (row in contact.fields.filter { it.kind == Kind.PHOTO }) {
                if (row.readOnly) throw Rejection("read_only_account")
                photos[row.rawContactId] = PhotoOp.Remove(row.dataId, row.version)
            }
        }
        Result.Planned(ops(), photos, photoRaw())
    } catch (rejection: Rejection) {
        Result.Rejected(rejection.reason)
    }

    private fun photoRaw(): Long? {
        val primary = contact.photoDataId?.let { id -> contact.fields.firstOrNull { it.dataId == id } }
        return primary?.takeIf { !it.readOnly }?.rawContactId ?: target?.rawContactId
    }

    private fun apply(patch: JSONObject, namePatched: Boolean) {
        val op = patch.optString("op")
        val path = patch.optString("path")
        val value: Any? = if (patch.has("value") && !patch.isNull("value")) patch.get("value") else null
        when {
            path == "display_name" -> if (!namePatched && op == "replace") scalar(Kind.NAME, StructuredName.DISPLAY_NAME, value as? String)
            path.startsWith("name.") -> {
                val column = ContactMapping.NAME_PARTS.firstOrNull { it.first == path.removePrefix("name.") }?.second ?: throw Rejection("unsupported_field")
                scalar(Kind.NAME, column, if (op == "replace") value as? String else null)
            }
            path == "nickname" -> if (op == "replace") scalar(Kind.NICKNAME, Nickname.NAME, value as? String) else removeScalar(Kind.NICKNAME)
            path == "organization" -> scalar(Kind.ORGANIZATION, Organization.COMPANY, if (op == "replace") value as? String else null)
            path == "title" -> scalar(Kind.ORGANIZATION, Organization.TITLE, if (op == "replace") value as? String else null)
            path == "notes" -> if (op == "replace") scalar(Kind.NOTE, Note.NOTE, value as? String) else removeScalar(Kind.NOTE)
            path == "birthday" -> birthday(op, value as? JSONObject)
            path in LISTS -> {
                if (op != "add" || value !is JSONObject) throw Rejection("unsupported_field")
                // Items beyond the core's 20 are invisible to it; never grow such a list blindly.
                if (contact.fields.count { it.kind == kindOf(path) } >= ContactMapping.MAX_LIST_VALUES) throw Rejection("unsupported_field")
                val raw = target?.rawContactId ?: throw Rejection("read_only_account")
                adds.getOrPut(raw) { mutableListOf() } += AndroidContactsProvider.NewField(kindOf(path), ContactMapping.listValues(kindOf(path), value))
            }
            else -> item(op, path, value as? JSONObject)
        }
    }

    private fun kindOf(list: String) = when (list) { "phones" -> Kind.PHONE; "emails" -> Kind.EMAIL; else -> Kind.POSTAL }

    /**
     * The row the capture displayed for a single-valued kind (same choice as [ContactMapping]).
     * Writing any other row would change something the user and the core never saw.
     */
    private fun shownRow(kind: Kind): Field? = ContactMapping.ordered(contact, kind).firstOrNull()

    private fun scalar(kind: Kind, column: String, value: String?) {
        val row = shownRow(kind)
        if (row != null) {
            if (row.readOnly) throw Rejection("read_only_account")
            // A value the core holds truncated must never be replaced from the truncated copy.
            if (!ContactMapping.representable(row)) throw Rejection("unsupported_field")
            rowValues.getOrPut(row.dataId) { mutableMapOf() }[column] = value
            return
        }
        if (value == null) {
            if (contact.fields.any { it.kind == kind }) throw Rejection("read_only_account")
            return
        }
        if (target == null) throw Rejection("read_only_account")
        scalarAdds.getOrPut(kind) { mutableMapOf() }[column] = value
    }

    private fun removeScalar(kind: Kind) {
        val rows = contact.fields.filter { it.kind == kind }
        if (rows.any { it.readOnly }) throw Rejection("read_only_account")
        if (!rows.all(ContactMapping::representable)) throw Rejection("unsupported_field")
        rows.forEach { removes += it.dataId }
    }

    private fun birthday(op: String, value: JSONObject?) {
        val rows = contact.fields.filter { it.kind == Kind.EVENT && it.values[Event.TYPE] == Event.TYPE_BIRTHDAY.toString() }
        // A birthday the core could not represent (e.g. year-only text) is never overwritten.
        if (!rows.all(ContactMapping::representable)) throw Rejection("unsupported_field")
        if (op == "remove") {
            if (rows.any { it.readOnly }) throw Rejection("read_only_account")
            rows.forEach { removes += it.dataId }
            return
        }
        val date = value?.let(ContactMapping::startDate) ?: throw Rejection("unsupported_field")
        val row = rows.firstOrNull { !it.readOnly }
        if (row != null) rowValues.getOrPut(row.dataId) { mutableMapOf() }[Event.START_DATE] = date
        else scalarAdds.getOrPut(Kind.EVENT) { mutableMapOf() }.apply { put(Event.START_DATE, date); put(Event.TYPE, Event.TYPE_BIRTHDAY.toString()) }
        if (row == null && target == null) throw Rejection("read_only_account")
    }

    private fun item(op: String, path: String, value: JSONObject?) {
        val open = path.indexOf('[')
        if (open <= 0 || !path.endsWith("]")) throw Rejection("unsupported_field")
        val list = path.substring(0, open)
        if (list !in LISTS) throw Rejection("unsupported_field")
        val coreId = path.substring(open + 1, path.length - 1)
        val native = toNative[list to coreId]?.toLongOrNull() ?: throw Rejection("stale_item")
        val row = contact.fields.firstOrNull { it.dataId == native && it.kind == kindOf(list) } ?: throw Rejection("stale_item")
        if (row.readOnly) throw Rejection("read_only_account")
        if (!ContactMapping.representable(row)) throw Rejection("unsupported_field")
        when (op) {
            "remove" -> removes += row.dataId
            "replace" -> rowValues.getOrPut(row.dataId) { mutableMapOf() }.putAll(ContactMapping.listValues(row.kind, value ?: throw Rejection("unsupported_field")))
            else -> throw Rejection("unsupported_field")
        }
    }

    private fun ops(): Map<Long, List<FieldOp>> {
        val out = LinkedHashMap<Long, MutableList<FieldOp>>()
        val byId = contact.fields.associateBy { it.dataId }
        for ((dataId, values) in rowValues) {
            if (dataId in removes) continue
            val row = byId.getValue(dataId)
            out.getOrPut(row.rawContactId) { mutableListOf() } += FieldOp.Replace(dataId, row.version, values)
        }
        for (dataId in removes) {
            val row = byId.getValue(dataId)
            out.getOrPut(row.rawContactId) { mutableListOf() } += FieldOp.Remove(dataId, row.version)
        }
        target?.rawContactId?.let { raw ->
            for ((kind, values) in scalarAdds) out.getOrPut(raw) { mutableListOf() } += FieldOp.Add(kind, values)
        }
        for ((raw, fields) in adds) for (field in fields) out.getOrPut(raw) { mutableListOf() } += FieldOp.Add(field.kind, field.values)
        return out
    }

    private companion object { val LISTS = setOf("phones", "emails", "addresses") }
}
