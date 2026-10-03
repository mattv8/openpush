package dev.openpush.mobile

import android.content.ContentProviderOperation
import android.content.ContentResolver
import android.content.ContentUris
import android.content.ContentValues
import android.content.Context
import android.content.OperationApplicationException
import android.os.Build
import android.provider.ContactsContract
import android.provider.ContactsContract.CommonDataKinds.Email
import android.provider.ContactsContract.CommonDataKinds.Event
import android.provider.ContactsContract.CommonDataKinds.Nickname
import android.provider.ContactsContract.CommonDataKinds.Note
import android.provider.ContactsContract.CommonDataKinds.Organization
import android.provider.ContactsContract.CommonDataKinds.Phone
import android.provider.ContactsContract.CommonDataKinds.Photo
import android.provider.ContactsContract.CommonDataKinds.StructuredName
import android.provider.ContactsContract.CommonDataKinds.StructuredPostal
import android.provider.ContactsContract.Contacts
import android.provider.ContactsContract.Data
import android.provider.ContactsContract.RawContacts
import androidx.annotation.RequiresApi
import java.io.ByteArrayOutputStream
import java.io.IOException

/**
 * Android Contacts adapter. It reads common fields with raw-contact/account provenance and stable
 * `Data._ID` item identities, and writes only individual raw contacts and data rows. It never uses
 * aggregate-contact update/delete shortcuts and never writes markers into provider accounts.
 *
 * The core owns stable contact IDs, revisions, permits, deletion inference and all durable state;
 * every [WriteResult] other than [WriteResult.Applied] / documented no-effect results must be
 * reconciled against a fresh [scan] by the caller.
 */
internal class AndroidContactsProvider(context: Context) {
    private val resolver: ContentResolver = context.contentResolver

    /** Common fields only. Column keys are the `ContactsContract` column constants for the kind. */
    enum class Kind(val mimeType: String, val columns: List<String>, val maxLength: Int = MAX_STRING, val maxItems: Int = MAX_ITEMS) {
        NAME(StructuredName.CONTENT_ITEM_TYPE, listOf(
            StructuredName.DISPLAY_NAME, StructuredName.GIVEN_NAME, StructuredName.FAMILY_NAME, StructuredName.PREFIX,
            StructuredName.MIDDLE_NAME, StructuredName.SUFFIX, StructuredName.PHONETIC_GIVEN_NAME,
            StructuredName.PHONETIC_MIDDLE_NAME, StructuredName.PHONETIC_FAMILY_NAME,
        ), maxItems = 1),
        NICKNAME(Nickname.CONTENT_ITEM_TYPE, listOf(Nickname.NAME, Nickname.TYPE, Nickname.LABEL)),
        PHONE(Phone.CONTENT_ITEM_TYPE, listOf(Phone.NUMBER, Phone.TYPE, Phone.LABEL)),
        EMAIL(Email.CONTENT_ITEM_TYPE, listOf(Email.ADDRESS, Email.TYPE, Email.LABEL)),
        ORGANIZATION(Organization.CONTENT_ITEM_TYPE, listOf(Organization.COMPANY, Organization.TYPE, Organization.LABEL, Organization.TITLE, Organization.DEPARTMENT)),
        POSTAL(StructuredPostal.CONTENT_ITEM_TYPE, listOf(
            StructuredPostal.FORMATTED_ADDRESS, StructuredPostal.TYPE, StructuredPostal.LABEL, StructuredPostal.STREET,
            StructuredPostal.POBOX, StructuredPostal.NEIGHBORHOOD, StructuredPostal.CITY, StructuredPostal.REGION,
            StructuredPostal.POSTCODE, StructuredPostal.COUNTRY,
        )),
        /** Birthday is `TYPE == Event.TYPE_BIRTHDAY`; START_DATE may be partial (`--MM-DD`). */
        EVENT(Event.CONTENT_ITEM_TYPE, listOf(Event.START_DATE, Event.TYPE, Event.LABEL)),
        NOTE(Note.CONTENT_ITEM_TYPE, listOf(Note.NOTE), maxLength = MAX_NOTE),
        /** Read-only provenance for the photo row; photo content is written via [PhotoOp]. */
        PHOTO(Photo.CONTENT_ITEM_TYPE, listOf(Photo.PHOTO_FILE_ID));

        /** Integer TYPE column (shared `data2` key) where the kind has one. */
        val typeColumn: String? get() = if (this == NAME || this == NOTE || this == PHOTO) null else Data.DATA2

        companion object { fun of(mimeType: String): Kind? = entries.firstOrNull { it.mimeType == mimeType } }
    }

    data class RawSource(val rawContactId: Long, val contactId: Long, val account: ContactAccount?, val writable: Boolean, val version: Int)
    data class Field(
        val dataId: Long,
        val rawContactId: Long,
        val kind: Kind,
        val values: Map<String, String?>,
        val readOnly: Boolean,
        val primary: Boolean,
        val version: Int,
    )
    data class Contact(
        val lookupKey: String,
        val contactId: Long,
        val displayName: String,
        val updatedAt: Long,
        /** `Data._ID` of the aggregate's chosen photo row, if any; matches a [Kind.PHOTO] field. */
        val photoDataId: Long?,
        val sources: List<RawSource>,
        val fields: List<Field>,
        /** A kind exceeded its item cap; the omitted values are unknown, not removed. */
        val truncated: Boolean,
    )

    sealed interface Page {
        /**
         * [hasMore] means continue from [nextAfterId]. [incomplete] means some rows in this page
         * could not be read: the overall scan must then be finished as non-authoritative.
         */
        data class Ready(val contacts: List<Contact>, val nextAfterId: Long?, val hasMore: Boolean, val incomplete: Boolean) : Page
        /** Nothing about absence may be inferred; report access unavailable or retry later. */
        data class Failed(val reason: Failure) : Page
    }
    enum class Failure { PERMISSION_DENIED, PROVIDER_UNAVAILABLE }

    sealed interface PhotoRead {
        data object None : PhotoRead
        class Bytes(val bytes: ByteArray) : PhotoRead
        data object TooLarge : PhotoRead
        /** Unreadable now; never interpret as a removed photo. */
        data object Failed : PhotoRead
    }

    sealed interface FieldOp {
        data class Add(val kind: Kind, val values: Map<String, String?>) : FieldOp
        /** Changes only the supplied columns of one existing row; other columns are preserved. */
        data class Replace(val dataId: Long, val expectedVersion: Int, val values: Map<String, String?>) : FieldOp
        data class Remove(val dataId: Long, val expectedVersion: Int) : FieldOp
    }
    sealed interface PhotoOp {
        /** Normalized JPEG (<= [MAX_NORMALIZED_PHOTO_BYTES]). */
        class Write(val jpeg: ByteArray) : PhotoOp
        data class Remove(val dataId: Long, val expectedVersion: Int) : PhotoOp
    }
    data class NewField(val kind: Kind, val values: Map<String, String?>)
    enum class PhotoOutcome { NOT_REQUESTED, WRITTEN, UNKNOWN }

    sealed interface WriteResult {
        /** Field changes committed. A requested photo may be [PhotoOutcome.UNKNOWN]: reread before reporting. */
        data class Applied(val rawContactId: Long, val photo: PhotoOutcome = PhotoOutcome.NOT_REQUESTED) : WriteResult
        /** No effect: the raw contact, row, or account is not writable by this app. */
        data object ReadOnly : WriteResult
        /** No effect: contacts permission is missing. */
        data object PermissionDenied : WriteResult
        /** No effect: the raw contact does not exist (or is deleted). */
        data object NotFound : WriteResult
        /** No effect: a target row changed or disappeared since it was read. */
        data object Conflict : WriteResult
        /** No effect: the request is malformed or exceeds bounds. */
        data class Invalid(val reason: String) : WriteResult
        /** Effect may or may not have happened; reconcile against provider evidence, never retry blindly. */
        data object OutcomeUnknown : WriteResult
    }

    sealed interface DefaultAccount {
        data object Local : DefaultAccount
        data class Cloud(val account: ContactAccount) : DefaultAccount
        /** Not exposed by this OS version, unset, SIM, or unreadable: use the explicit app setting. */
        data object Unavailable : DefaultAccount
    }

    // ---- accounts -----------------------------------------------------------------------------

    /**
     * Accounts that can receive new contacts: those with existing raw contacts whose contacts sync
     * adapter supports uploading, plus the OS default cloud account. Read-only sync accounts
     * (messaging apps etc.) are excluded. Performs provider I/O; call off the main thread.
     */
    fun accounts(): List<ContactAccount> {
        val uploadable = uploadableAccountTypes()
        val found = LinkedHashSet<ContactAccount>()
        resolver.query(RawContacts.CONTENT_URI, arrayOf(RawContacts.ACCOUNT_NAME, RawContacts.ACCOUNT_TYPE), "${RawContacts.DELETED}=0", null, null)?.use { c ->
            while (c.moveToNext()) {
                val name = c.getString(0) ?: continue
                val type = c.getString(1) ?: continue
                if (type in uploadable) found += ContactAccount(name, type)
            }
        } ?: throw IOException("contacts provider unavailable")
        (osDefaultAccount() as? DefaultAccount.Cloud)?.account?.takeIf { it.type in uploadable }?.let(found::add)
        return found.toList()
    }

    /** The OS-selected account for new contacts where the platform exposes it. */
    fun osDefaultAccount(): DefaultAccount = try {
        when {
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.BAKLAVA -> Api36.defaultAccount(resolver)
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU -> Api33.defaultAccount(resolver)
            else -> DefaultAccount.Unavailable
        }
    } catch (_: Exception) {
        DefaultAccount.Unavailable
    }

    // ---- reads --------------------------------------------------------------------------------

    /**
     * Reads up to [limit] aggregate contacts with `_ID > afterContactId`, in ascending ID order.
     * With [updatedSince] only contacts whose `CONTACT_LAST_UPDATED_TIMESTAMP` is newer are read
     * (an incremental hint; never authoritative for absence).
     */
    fun scan(afterContactId: Long?, limit: Int, updatedSince: Long? = null): Page = try {
        val capped = limit.coerceIn(1, MAX_PAGE)
        val heads = mutableListOf<Head>()
        var lastSeen: Long? = afterContactId
        var incomplete = false
        var hasMore = false
        val clauses = listOfNotNull(
            afterContactId?.let { "${Contacts._ID} > ?" },
            updatedSince?.let { "${Contacts.CONTACT_LAST_UPDATED_TIMESTAMP} > ?" },
        )
        val args = listOfNotNull(afterContactId?.toString(), updatedSince?.toString()).toTypedArray()
        val cursor = resolver.query(
            Contacts.CONTENT_URI, CONTACT_PROJECTION, clauses.joinToString(" AND ").ifEmpty { null },
            args.takeIf { it.isNotEmpty() }, "${Contacts._ID} ASC",
        ) ?: return Page.Failed(Failure.PROVIDER_UNAVAILABLE)
        cursor.use { c ->
            var seen = 0
            while (c.moveToNext()) {
                if (seen == capped) { hasMore = true; break }
                seen++
                val id = c.getLong(0)
                lastSeen = id
                val head = head(c)
                if (head == null) { incomplete = true; continue }
                heads += head
            }
        }
        val contacts = assemble(heads)
        if (contacts.size < heads.size) incomplete = true
        Page.Ready(contacts, lastSeen, hasMore, incomplete)
    } catch (_: SecurityException) {
        Page.Failed(Failure.PERMISSION_DENIED)
    } catch (_: Exception) {
        Page.Failed(Failure.PROVIDER_UNAVAILABLE)
    }

    sealed interface Lookup {
        data class Found(val contact: Contact) : Lookup
        /** The provider answered and the contact does not exist (any more). */
        data object Missing : Lookup
        /** Unreadable now; never evidence of absence. */
        data class Failed(val reason: Failure) : Lookup
    }

    /** Current aggregate for a lookup key; follows re-aggregation via `CONTENT_LOOKUP_URI`. */
    fun contactByLookup(lookupKey: String): Lookup = lookupRead {
        val uri = android.net.Uri.withAppendedPath(Contacts.CONTENT_LOOKUP_URI, lookupKey)
        resolver.query(uri, CONTACT_PROJECTION, null, null, null)
    }

    /** Current aggregate containing a live raw contact. */
    fun contactByRawContact(rawContactId: Long): Lookup = try {
        val contactId = resolver.query(
            ContentUris.withAppendedId(RawContacts.CONTENT_URI, rawContactId), arrayOf(RawContacts.CONTACT_ID, RawContacts.DELETED), null, null, null,
        )?.use { c -> if (c.moveToFirst() && c.getInt(1) == 0 && !c.isNull(0)) c.getLong(0) else -1L } ?: throw IOException("contacts provider unavailable")
        if (contactId < 0) Lookup.Missing else lookupRead {
            resolver.query(ContentUris.withAppendedId(Contacts.CONTENT_URI, contactId), CONTACT_PROJECTION, null, null, null)
        }
    } catch (_: SecurityException) {
        Lookup.Failed(Failure.PERMISSION_DENIED)
    } catch (_: Exception) {
        Lookup.Failed(Failure.PROVIDER_UNAVAILABLE)
    }

    /**
     * Number of aggregate deletions the provider recorded after [sinceMillis], or null when the
     * history is unreadable. A hint that a full authoritative scan is due, never a deletion itself.
     */
    fun deletedSince(sinceMillis: Long): Int? = try {
        resolver.query(
            ContactsContract.DeletedContacts.CONTENT_URI, arrayOf(ContactsContract.DeletedContacts.CONTACT_ID),
            "${ContactsContract.DeletedContacts.CONTACT_DELETED_TIMESTAMP} > ?", arrayOf(sinceMillis.toString()), null,
        )?.use { it.count }
    } catch (_: Exception) {
        null
    }

    private inline fun lookupRead(query: () -> android.database.Cursor?): Lookup = try {
        val head = (query() ?: throw IOException("contacts provider unavailable")).use { c -> if (c.moveToFirst()) head(c) else null }
        if (head == null) Lookup.Missing else assemble(listOf(head)).singleOrNull()?.let { Lookup.Found(it) } ?: Lookup.Missing
    } catch (_: SecurityException) {
        Lookup.Failed(Failure.PERMISSION_DENIED)
    } catch (_: IllegalArgumentException) {
        // An unknown/garbled lookup key is reported by the provider as an illegal argument.
        Lookup.Missing
    } catch (_: Exception) {
        Lookup.Failed(Failure.PROVIDER_UNAVAILABLE)
    }

    private fun head(c: android.database.Cursor): Head? {
        val lookup = c.getString(1) ?: return null
        return Head(c.getLong(0), lookup, c.getString(2).orEmpty(), c.getLong(3), if (c.isNull(4)) null else c.getLong(4))
    }

    /** Contacts for [heads] with batched raw/data reads; heads without live raws are dropped. */
    private fun assemble(heads: List<Head>): List<Contact> {
        val uploadable = uploadableAccountTypes()
        val sourcesByContact = rawSources("${RawContacts.CONTACT_ID} IN", heads.map { it.id }, uploadable).groupBy { it.contactId }
        val sourceById = sourcesByContact.values.flatten().associateBy { it.rawContactId }
        val fieldsByRaw = fields(sourceById.values).groupBy { it.rawContactId }
        return heads.mapNotNull { head ->
            val sources = sourcesByContact[head.id].orEmpty()
            if (sources.isEmpty()) return@mapNotNull null
            val all = sources.flatMap { fieldsByRaw[it.rawContactId].orEmpty() }
            val kept = all.groupBy { it.kind }.flatMap { (_, rows) -> rows.take(MAX_ITEMS) }
            Contact(head.lookup, head.id, head.name, head.updatedAt, head.photoId, sources, kept, kept.size < all.size)
        }
    }

    /** Bounded high-resolution read of the aggregate photo (decode input limit). */
    fun readPhoto(contactId: Long, maxBytes: Int = MAX_DECODE_BYTES): PhotoRead = try {
        val input = Contacts.openContactPhotoInputStream(resolver, ContentUris.withAppendedId(Contacts.CONTENT_URI, contactId), true)
        if (input == null) PhotoRead.None else input.use {
            val out = ByteArrayOutputStream()
            val buffer = ByteArray(8192)
            var tooLarge = false
            while (true) {
                val n = it.read(buffer)
                if (n < 0) break
                if (out.size() + n > maxBytes) { tooLarge = true; break }
                out.write(buffer, 0, n)
            }
            if (tooLarge) PhotoRead.TooLarge else PhotoRead.Bytes(out.toByteArray())
        }
    } catch (_: SecurityException) {
        PhotoRead.Failed
    } catch (_: IOException) {
        PhotoRead.Failed
    }

    // ---- writes -------------------------------------------------------------------------------

    /** Applies field-level changes to one raw contact in a single provider transaction. */
    fun update(rawContactId: Long, ops: List<FieldOp>, photo: PhotoOp? = null): WriteResult {
        if (ops.isEmpty() && photo == null) return WriteResult.Invalid("empty update")
        ops.forEach { op ->
            when (op) {
                is FieldOp.Add -> validate(op.kind, op.values, adding = true)?.let { return WriteResult.Invalid(it) }
                is FieldOp.Replace -> if (op.values.isEmpty()) return WriteResult.Invalid("empty replace")
                is FieldOp.Remove -> Unit
            }
        }
        validatePhoto(photo)?.let { return WriteResult.Invalid(it) }
        val targets = ops.mapNotNull { (it as? FieldOp.Replace)?.dataId ?: (it as? FieldOp.Remove)?.dataId } + listOfNotNull((photo as? PhotoOp.Remove)?.dataId)
        if (targets.size != targets.toSet().size) return WriteResult.Invalid("duplicate target row")

        return try {
            val raw = rawSources("${RawContacts._ID} IN", listOf(rawContactId), uploadableAccountTypes()).singleOrNull()
                ?: return WriteResult.NotFound
            if (!raw.writable) return WriteResult.ReadOnly
            val current = fields(listOf(raw)).associateBy { it.dataId }
            val counts = current.values.groupingBy { it.kind }.eachCount().toMutableMap()
            val batch = ArrayList<ContentProviderOperation>()
            for (op in ops) {
                when (op) {
                    is FieldOp.Add -> {
                        counts[op.kind] = (counts[op.kind] ?: 0) + 1
                        batch += ContentProviderOperation.newInsert(Data.CONTENT_URI)
                            .withValue(Data.RAW_CONTACT_ID, rawContactId)
                            .withValue(Data.MIMETYPE, op.kind.mimeType)
                            .withValues(contentValues(op.values))
                            .build()
                    }
                    is FieldOp.Replace -> {
                        val row = current[op.dataId] ?: return WriteResult.Conflict
                        validate(row.kind, op.values, adding = false)?.let { return WriteResult.Invalid(it) }
                        if (row.readOnly) return WriteResult.ReadOnly
                        if (row.version != op.expectedVersion) return WriteResult.Conflict
                        batch += ContentProviderOperation.newUpdate(Data.CONTENT_URI)
                            .withSelection(GUARDED_ROW, guardArgs(row))
                            .withValues(contentValues(op.values))
                            .withExpectedCount(1)
                            .build()
                    }
                    is FieldOp.Remove -> {
                        val row = current[op.dataId] ?: return WriteResult.Conflict
                        if (row.kind == Kind.PHOTO) return WriteResult.Invalid("photo rows change via PhotoOp")
                        if (row.readOnly) return WriteResult.ReadOnly
                        if (row.version != op.expectedVersion) return WriteResult.Conflict
                        counts[row.kind] = (counts[row.kind] ?: 1) - 1
                        batch += ContentProviderOperation.newDelete(Data.CONTENT_URI)
                            .withSelection(GUARDED_ROW, guardArgs(row))
                            .withExpectedCount(1)
                            .build()
                    }
                }
            }
            when (photo) {
                is PhotoOp.Remove -> {
                    val row = current[photo.dataId]?.takeIf { it.kind == Kind.PHOTO } ?: return WriteResult.Conflict
                    if (row.readOnly) return WriteResult.ReadOnly
                    if (row.version != photo.expectedVersion) return WriteResult.Conflict
                    batch += ContentProviderOperation.newDelete(Data.CONTENT_URI).withSelection(GUARDED_ROW, guardArgs(row)).withExpectedCount(1).build()
                }
                is PhotoOp.Write -> if (current.values.any { it.kind == Kind.PHOTO && it.readOnly }) return WriteResult.ReadOnly
                null -> Unit
            }
            // Only kinds this request grows are capped; pre-existing oversized lists stay editable.
            ops.filterIsInstance<FieldOp.Add>().map { it.kind }.distinct().firstOrNull { (counts[it] ?: 0) > it.maxItems }
                ?.let { return WriteResult.Invalid("too many ${it.name.lowercase()} values") }

            if (batch.isNotEmpty()) {
                try {
                    resolver.applyBatch(ContactsContract.AUTHORITY, batch)
                } catch (_: OperationApplicationException) {
                    // ContactsProvider applies a batch without yield points in one transaction, so a
                    // failed expected-count guard rolls everything back.
                    return WriteResult.Conflict
                }
            }
            val photoOutcome = when (photo) {
                is PhotoOp.Write -> writeDisplayPhoto(rawContactId, photo.jpeg)
                is PhotoOp.Remove -> PhotoOutcome.WRITTEN
                null -> PhotoOutcome.NOT_REQUESTED
            }
            if (batch.isEmpty() && photoOutcome == PhotoOutcome.UNKNOWN) WriteResult.OutcomeUnknown
            else WriteResult.Applied(rawContactId, photoOutcome)
        } catch (_: SecurityException) {
            WriteResult.PermissionDenied
        } catch (_: Exception) {
            WriteResult.OutcomeUnknown
        }
    }

    /**
     * Creates one raw contact in [account] (`null` = device-local). Before calling, the caller must
     * durably record [findCreateMatches] for the same arguments; on [WriteResult.OutcomeUnknown] it
     * adopts a post-create match only if exactly one matching raw contact is new.
     */
    fun create(account: ContactAccount?, fields: List<NewField>, photo: ByteArray? = null): WriteResult {
        if (fields.none { field -> field.values.values.any { !it.isNullOrBlank() } }) return WriteResult.Invalid("empty contact")
        fields.forEach { validate(it.kind, it.values, adding = true)?.let { reason -> return WriteResult.Invalid(reason) } }
        fields.groupingBy { it.kind }.eachCount().entries.firstOrNull { (kind, count) -> count > kind.maxItems }
            ?.let { return WriteResult.Invalid("too many ${it.key.name.lowercase()} values") }
        photo?.let { validatePhoto(PhotoOp.Write(it)) }?.let { return WriteResult.Invalid(it) }
        return try {
            if (account != null && account.type !in uploadableAccountTypes()) return WriteResult.ReadOnly
            val batch = ArrayList<ContentProviderOperation>()
            batch += ContentProviderOperation.newInsert(RawContacts.CONTENT_URI).withValues(ContentValues().apply {
                if (account != null) {
                    put(RawContacts.ACCOUNT_NAME, account.name)
                    put(RawContacts.ACCOUNT_TYPE, account.type)
                }
            }).build()
            fields.forEach { field ->
                batch += ContentProviderOperation.newInsert(Data.CONTENT_URI)
                    .withValueBackReference(Data.RAW_CONTACT_ID, 0)
                    .withValue(Data.MIMETYPE, field.kind.mimeType)
                    .withValues(contentValues(field.values))
                    .build()
            }
            val results = try {
                resolver.applyBatch(ContactsContract.AUTHORITY, batch)
            } catch (_: OperationApplicationException) {
                return WriteResult.Invalid("provider rejected contact")
            }
            val rawId = results.firstOrNull()?.uri?.let { runCatching { ContentUris.parseId(it) }.getOrNull() }
                ?.takeIf { it > 0 } ?: return WriteResult.OutcomeUnknown
            WriteResult.Applied(rawId, if (photo == null) PhotoOutcome.NOT_REQUESTED else writeDisplayPhoto(rawId, photo))
        } catch (_: SecurityException) {
            WriteResult.PermissionDenied
        } catch (_: Exception) {
            WriteResult.OutcomeUnknown
        }
    }

    /**
     * Deletes one writable raw contact (other raw contacts in the aggregate are untouched). The
     * provider marks it DELETED for its sync adapter; [expectedVersion] guards stale requests.
     */
    fun delete(rawContactId: Long, expectedVersion: Int): WriteResult = try {
        val raw = rawSources("${RawContacts._ID} IN", listOf(rawContactId), uploadableAccountTypes()).singleOrNull()
        when {
            raw == null -> WriteResult.NotFound
            !raw.writable -> WriteResult.ReadOnly
            raw.version != expectedVersion -> WriteResult.Conflict
            else -> {
                val uri = ContentUris.withAppendedId(RawContacts.CONTENT_URI, rawContactId)
                when (resolver.delete(uri, "${RawContacts.VERSION}=?", arrayOf(expectedVersion.toString()))) {
                    1 -> WriteResult.Applied(rawContactId)
                    0 -> WriteResult.Conflict
                    else -> WriteResult.OutcomeUnknown
                }
            }
        }
    } catch (_: SecurityException) {
        WriteResult.PermissionDenied
    } catch (_: Exception) {
        WriteResult.OutcomeUnknown
    }

    /**
     * Live raw contacts in [account] whose common fields exactly match [fields] on every supplied
     * value (same item count per kind). Returns null when evidence cannot be read or no supplied
     * field can anchor the search. Never writes anything.
     */
    fun findCreateMatches(account: ContactAccount?, fields: List<NewField>): Set<Long>? = try {
        // Anchor on any supplied value column (e.g. GIVEN_NAME when a name has only parts; the
        // provider derives DISPLAY_NAME/DATA1 itself), never on a TYPE code.
        val (anchorKind, anchorColumn, anchorValue) = fields.firstNotNullOfOrNull { field ->
            field.kind.columns.filter { it != field.kind.typeColumn }
                .firstNotNullOfOrNull { column -> field.values[column]?.takeIf { it.isNotEmpty() }?.let { Triple(field.kind, column, it) } }
        } ?: return null
        val ids = mutableSetOf<Long>()
        resolver.query(Data.CONTENT_URI, arrayOf(Data.RAW_CONTACT_ID), "${Data.MIMETYPE}=? AND $anchorColumn=?", arrayOf(anchorKind.mimeType, anchorValue), null)
            ?.use { c -> while (c.moveToNext()) ids += c.getLong(0) } ?: throw IOException("contacts provider unavailable")
        val raws = rawSources("${RawContacts._ID} IN", ids.toList(), uploadableAccountTypes()).filter { it.account == account }
        val byRaw = fields(raws).groupBy { it.rawContactId }
        raws.filter { raw -> sameContent(byRaw[raw.rawContactId].orEmpty(), fields) }.mapTo(mutableSetOf()) { it.rawContactId }
    } catch (_: Exception) {
        null
    }

    // ---- internals ----------------------------------------------------------------------------

    private data class Head(val id: Long, val lookup: String, val name: String, val updatedAt: Long, val photoId: Long?)

    private fun sameContent(existing: List<Field>, expected: List<NewField>): Boolean {
        val existingByKind = existing.filter { it.kind != Kind.PHOTO }.groupBy { it.kind }
        val expectedByKind = expected.groupBy { it.kind }
        if (existingByKind.keys != expectedByKind.keys) return false
        return expectedByKind.all { (kind, wanted) ->
            val have = existingByKind.getValue(kind).toMutableList()
            have.size == wanted.size && wanted.all { field ->
                val match = have.firstOrNull { row -> field.values.all { (column, value) -> value == null || row.values[column] == value } }
                match != null && have.remove(match)
            }
        }
    }

    private fun validate(kind: Kind, values: Map<String, String?>, adding: Boolean): String? {
        if (kind == Kind.PHOTO) return "photo rows change via PhotoOp"
        if (adding && values.values.all { it.isNullOrBlank() }) return "empty ${kind.name.lowercase()}"
        values.forEach { (column, value) ->
            if (column !in kind.columns) return "unsupported ${kind.name.lowercase()} column $column"
            if (value == null) return@forEach
            if (value.length > kind.maxLength) return "${kind.name.lowercase()} value too long"
            if (column == kind.typeColumn && value.toIntOrNull() == null) return "non-numeric type"
            if (kind == Kind.EVENT && column == Event.START_DATE && !PARTIAL_DATE.matches(value)) return "invalid date"
        }
        return null
    }

    private fun validatePhoto(photo: PhotoOp?): String? = when (photo) {
        is PhotoOp.Write -> when {
            photo.jpeg.isEmpty() || photo.jpeg.size > MAX_NORMALIZED_PHOTO_BYTES -> "photo must be a normalized JPEG <= 64KiB"
            photo.jpeg.size < 2 || photo.jpeg[0] != 0xFF.toByte() || photo.jpeg[1] != 0xD8.toByte() -> "photo must be JPEG"
            else -> null
        }
        else -> null
    }

    private fun contentValues(values: Map<String, String?>) = ContentValues().apply {
        values.forEach { (column, value) -> if (value == null) putNull(column) else put(column, value) }
    }

    private fun guardArgs(row: Field) = arrayOf(row.dataId.toString(), row.rawContactId.toString(), row.version.toString())

    /** Writes through the DisplayPhoto stream (binder-safe); the provider derives the thumbnail. */
    private fun writeDisplayPhoto(rawContactId: Long, jpeg: ByteArray): PhotoOutcome = try {
        val uri = ContentUris.withAppendedId(RawContacts.CONTENT_URI, rawContactId).buildUpon()
            .appendPath(RawContacts.DisplayPhoto.CONTENT_DIRECTORY).build()
        val descriptor = resolver.openAssetFileDescriptor(uri, "rw")
        if (descriptor == null) PhotoOutcome.UNKNOWN else {
            descriptor.use { fd -> fd.createOutputStream().use { it.write(jpeg) } }
            PhotoOutcome.WRITTEN
        }
    } catch (_: Exception) {
        PhotoOutcome.UNKNOWN
    }

    private fun uploadableAccountTypes(): Set<String> = ContentResolver.getSyncAdapterTypes()
        .filter { it.authority == ContactsContract.AUTHORITY && it.supportsUploading() }
        .mapTo(HashSet()) { it.accountType }

    /** Live (non-deleted) raw contacts where [columnIn] (an `X IN` prefix) matches [ids], chunked. */
    private fun rawSources(columnIn: String, ids: List<Long>, uploadable: Set<String>): List<RawSource> {
        val out = mutableListOf<RawSource>()
        ids.distinct().chunked(MAX_SQL_ARGS).forEach { chunk ->
            val selection = "$columnIn (${chunk.joinToString(",") { "?" }}) AND ${RawContacts.DELETED}=0"
            resolver.query(RawContacts.CONTENT_URI, RAW_PROJECTION, selection, chunk.map(Long::toString).toTypedArray(), "${RawContacts._ID} ASC")?.use { c ->
                while (c.moveToNext()) {
                    val name = c.getString(2)
                    val type = c.getString(3)
                    val account = if (name != null && type != null) ContactAccount(name, type) else null
                    // Rows with only one account column set are malformed: fail closed.
                    val accountWritable = if (account == null) name == null && type == null else type in uploadable
                    out += RawSource(c.getLong(0), c.getLong(1), account, c.getInt(4) == 0 && accountWritable, c.getInt(5))
                }
            } ?: throw IOException("contacts provider unavailable")
        }
        return out
    }

    /** All common-field rows of [raws]; read-only when the row or its raw contact is read-only. */
    private fun fields(raws: Collection<RawSource>): List<Field> {
        val writable = raws.associate { it.rawContactId to it.writable }
        val out = mutableListOf<Field>()
        val mimeArgs = Kind.entries.map { it.mimeType }
        writable.keys.toList().chunked(MAX_SQL_ARGS - mimeArgs.size).forEach { chunk ->
            val selection = "${Data.RAW_CONTACT_ID} IN (${chunk.joinToString(",") { "?" }}) AND ${Data.MIMETYPE} IN (${mimeArgs.joinToString(",") { "?" }})"
            val args = (chunk.map(Long::toString) + mimeArgs).toTypedArray()
            resolver.query(Data.CONTENT_URI, DATA_PROJECTION, selection, args, "${Data.RAW_CONTACT_ID} ASC, ${Data.IS_PRIMARY} DESC, ${Data._ID} ASC")?.use { c ->
                while (c.moveToNext()) {
                    val kind = Kind.of(c.getString(2) ?: continue) ?: continue
                    val rawId = c.getLong(1)
                    val values = kind.columns.associateWith { column -> c.getString(DATA_PROJECTION.indexOf(column)) }
                    out += Field(c.getLong(0), rawId, kind, values, c.getInt(3) != 0 || writable[rawId] != true, c.getInt(4) != 0, c.getInt(5))
                }
            } ?: throw IOException("contacts provider unavailable")
        }
        return out
    }

    @RequiresApi(Build.VERSION_CODES.TIRAMISU)
    private object Api33 {
        @Suppress("DEPRECATION")
        fun defaultAccount(resolver: ContentResolver): DefaultAccount =
            ContactsContract.Settings.getDefaultAccount(resolver)?.let { DefaultAccount.Cloud(ContactAccount(it.name, it.type)) }
                ?: DefaultAccount.Unavailable // null is documented for both "local" and "not set"
    }

    @RequiresApi(Build.VERSION_CODES.BAKLAVA)
    private object Api36 {
        fun defaultAccount(resolver: ContentResolver): DefaultAccount {
            val state = RawContacts.DefaultAccount.getDefaultAccountForNewContacts(resolver)
            val account = state.account
            return when (state.state) {
                RawContacts.DefaultAccount.DefaultAccountAndState.DEFAULT_ACCOUNT_STATE_LOCAL -> DefaultAccount.Local
                RawContacts.DefaultAccount.DefaultAccountAndState.DEFAULT_ACCOUNT_STATE_CLOUD ->
                    if (account == null) DefaultAccount.Unavailable else DefaultAccount.Cloud(ContactAccount(account.name, account.type))
                else -> DefaultAccount.Unavailable
            }
        }
    }

    companion object {
        const val MAX_ITEMS = 20
        const val MAX_STRING = 1024
        const val MAX_NOTE = 8 * 1024
        const val MAX_PAGE = 200
        const val MAX_DECODE_BYTES = 8 * 1024 * 1024
        const val MAX_NORMALIZED_PHOTO_BYTES = 64 * 1024
        private const val MAX_SQL_ARGS = 500
        private val PARTIAL_DATE = Regex("""^(\d{4}|-)-\d{2}-\d{2}$""")
        private val GUARDED_ROW = "${Data._ID}=? AND ${Data.RAW_CONTACT_ID}=? AND ${Data.DATA_VERSION}=?"
        private val CONTACT_PROJECTION = arrayOf(Contacts._ID, Contacts.LOOKUP_KEY, Contacts.DISPLAY_NAME, Contacts.CONTACT_LAST_UPDATED_TIMESTAMP, Contacts.PHOTO_ID)
        private val RAW_PROJECTION = arrayOf(RawContacts._ID, RawContacts.CONTACT_ID, RawContacts.ACCOUNT_NAME, RawContacts.ACCOUNT_TYPE, RawContacts.RAW_CONTACT_IS_READ_ONLY, RawContacts.VERSION)
        private val DATA_PROJECTION = arrayOf(
            Data._ID, Data.RAW_CONTACT_ID, Data.MIMETYPE, Data.IS_READ_ONLY, Data.IS_PRIMARY, Data.DATA_VERSION,
            Data.DATA1, Data.DATA2, Data.DATA3, Data.DATA4, Data.DATA5, Data.DATA6, Data.DATA7, Data.DATA8, Data.DATA9, Data.DATA10, Data.DATA14,
        )
    }
}
