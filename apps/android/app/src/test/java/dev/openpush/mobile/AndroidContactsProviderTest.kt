package dev.openpush.mobile

import android.content.ContentProvider
import android.content.ContentProviderOperation
import android.content.ContentProviderResult
import android.content.ContentValues
import android.content.SyncAdapterType
import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import android.os.Looper
import android.provider.ContactsContract
import android.provider.ContactsContract.CommonDataKinds.Email
import android.provider.ContactsContract.CommonDataKinds.Event
import android.provider.ContactsContract.CommonDataKinds.Note
import android.provider.ContactsContract.CommonDataKinds.Phone
import android.provider.ContactsContract.CommonDataKinds.StructuredName
import androidx.test.core.app.ApplicationProvider
import dev.openpush.mobile.AndroidContactsProvider.FieldOp
import dev.openpush.mobile.AndroidContactsProvider.Kind
import dev.openpush.mobile.AndroidContactsProvider.NewField
import dev.openpush.mobile.AndroidContactsProvider.Page
import dev.openpush.mobile.AndroidContactsProvider.WriteResult
import java.time.Duration
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowContentResolver

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class AndroidContactsProviderTest {
    private lateinit var fake: FakeContactsProvider
    private lateinit var contacts: AndroidContactsProvider

    @Before fun setUp() {
        fake = Robolectric.buildContentProvider(FakeContactsProvider::class.java).create().get()
        ShadowContentResolver.registerProviderInternal(ContactsContract.AUTHORITY, fake)
        ShadowContentResolver.setSyncAdapterTypes(arrayOf(
            SyncAdapterType(ContactsContract.AUTHORITY, GOOGLE, true, true),
            SyncAdapterType(ContactsContract.AUTHORITY, MESSENGER, false, false),
        ))
        contacts = AndroidContactsProvider(ApplicationProvider.getApplicationContext())
        // Contact 1 aggregates a local raw (10), an uploadable cloud raw (11) and a read-only sync raw (12).
        fake.contact(1, "Ada Lovelace")
        fake.raw(10, contactId = 1, account = null)
        fake.raw(11, contactId = 1, account = ContactAccount("ada@example.test", GOOGLE))
        fake.raw(12, contactId = 1, account = ContactAccount("ada", MESSENGER))
        fake.data(100, 10, StructuredName.CONTENT_ITEM_TYPE, "data1" to "Ada Lovelace", "data2" to "Ada", "data3" to "Lovelace")
        fake.data(101, 10, Phone.CONTENT_ITEM_TYPE, "data1" to "+12025550123", "data2" to "${Phone.TYPE_MOBILE}", primary = true)
        fake.data(102, 11, Email.CONTENT_ITEM_TYPE, "data1" to "ada@example.test", "data2" to "${Email.TYPE_HOME}")
        fake.data(103, 12, Phone.CONTENT_ITEM_TYPE, "data1" to "+12025550999", "data2" to "${Phone.TYPE_CUSTOM}", "data3" to "Messenger")
        fake.data(104, 10, Event.CONTENT_ITEM_TYPE, "data1" to "--12-10", "data2" to "${Event.TYPE_BIRTHDAY}")
        fake.data(105, 10, "vnd.android.cursor.item/website", "data1" to "https://example.test")
    }

    @Test fun scanKeepsFieldProvenanceTypedValuesAndWritability() {
        val page = contacts.scan(null, 10) as Page.Ready
        val contact = page.contacts.single()
        val sources = contact.sources.associateBy { it.rawContactId }
        assertTrue(sources.getValue(10).writable)
        assertTrue(sources.getValue(11).writable)
        assertFalse(sources.getValue(12).writable)
        val fields = contact.fields.associateBy { it.dataId }
        assertEquals(setOf(100L, 101L, 102L, 103L, 104L), fields.keys) // unknown mimetypes are not exposed
        assertEquals(10L, fields.getValue(101).rawContactId)
        assertEquals("${Phone.TYPE_MOBILE}", fields.getValue(101).values[Phone.TYPE])
        assertTrue(fields.getValue(101).primary)
        assertEquals("Lovelace", fields.getValue(100).values[StructuredName.FAMILY_NAME])
        assertEquals("Messenger", fields.getValue(103).values[Phone.LABEL])
        assertTrue(fields.getValue(103).readOnly)
        assertFalse(fields.getValue(102).readOnly)
        assertEquals("--12-10", fields.getValue(104).values[Event.START_DATE])
        assertFalse(contact.truncated)
        assertFalse(page.incomplete)
    }

    @Test fun accountsOfferOnlyUploadableAccounts() {
        assertEquals(listOf(ContactAccount("ada@example.test", GOOGLE)), contacts.accounts())
    }

    @Test fun pagingReportsContinuationAndFinalPage() {
        fake.contact(2, "Second"); fake.raw(20, contactId = 2, account = null)
        val first = contacts.scan(null, 1) as Page.Ready
        assertTrue(first.hasMore)
        assertEquals(1L, first.nextAfterId)
        val last = contacts.scan(first.nextAfterId, 1) as Page.Ready
        assertEquals(2L, last.contacts.single().contactId)
        assertFalse(last.hasMore)
    }

    @Test fun providerFailuresAreNeverAnEmptyTerminalPage() {
        fake.nullCursorFor = "contacts"
        assertEquals(Page.Failed(AndroidContactsProvider.Failure.PROVIDER_UNAVAILABLE), contacts.scan(null, 10))
        fake.nullCursorFor = "data"
        assertEquals(Page.Failed(AndroidContactsProvider.Failure.PROVIDER_UNAVAILABLE), contacts.scan(null, 10))
        fake.nullCursorFor = null
        fake.denyAccess = true
        assertEquals(Page.Failed(AndroidContactsProvider.Failure.PERMISSION_DENIED), contacts.scan(null, 10))
    }

    @Test fun unreadableRowsMakeThePageIncompleteButStillProgress() {
        fake.contact(2, "No lookup", lookup = null)
        fake.contact(3, "Third"); fake.raw(30, contactId = 3, account = null)
        val page = contacts.scan(null, 10) as Page.Ready
        assertTrue(page.incomplete)
        assertEquals(listOf(1L, 3L), page.contacts.map { it.contactId })
        assertEquals(3L, page.nextAfterId)
    }

    @Test fun perKindCapMarksContactTruncated() {
        repeat(25) { fake.data(200L + it, 10, Phone.CONTENT_ITEM_TYPE, "data1" to "+1202555${1000 + it}", "data2" to "2") }
        val contact = (contacts.scan(null, 10) as Page.Ready).contacts.single()
        assertEquals(AndroidContactsProvider.MAX_ITEMS, contact.fields.count { it.kind == Kind.PHONE })
        assertTrue(contact.truncated)
    }

    @Test fun replacePreservesUntouchedColumnsAndBumpsVersion() {
        val result = contacts.update(10, listOf(FieldOp.Replace(101, 0, mapOf(Phone.NUMBER to "+12025550124"))))
        assertEquals(WriteResult.Applied(10), result)
        val row = fake.row(101)
        assertEquals("+12025550124", row["data1"])
        assertEquals("${Phone.TYPE_MOBILE}", row["data2"])
        assertEquals(1, row["data_version"])
    }

    @Test fun staleVersionIsConflictWithoutMutation() {
        assertEquals(WriteResult.Conflict, contacts.update(10, listOf(FieldOp.Replace(101, 7, mapOf(Phone.NUMBER to "x")))))
        assertEquals(WriteResult.Conflict, contacts.update(10, listOf(FieldOp.Remove(102, 0)))) // row of another raw contact
        assertEquals(0, fake.mutations)
    }

    @Test fun guardFailureInsideProviderRollsBackAsConflict() {
        fake.bumpVersionBeforeBatch = 101
        val result = contacts.update(10, listOf(FieldOp.Add(Kind.NOTE, mapOf(Note.NOTE to "n")), FieldOp.Replace(101, 0, mapOf(Phone.NUMBER to "x"))))
        assertEquals(WriteResult.Conflict, result)
        assertTrue(fake.rows.none { it["mimetype"] == Note.CONTENT_ITEM_TYPE })
    }

    @Test fun readOnlyRawsAndRowsAreRejectedBeforeProviderMutation() {
        assertEquals(WriteResult.ReadOnly, contacts.update(12, listOf(FieldOp.Replace(103, 0, mapOf(Phone.NUMBER to "x")))))
        assertEquals(WriteResult.ReadOnly, contacts.delete(12, 0))
        fake.row(100)["is_read_only"] = 1
        assertEquals(WriteResult.ReadOnly, contacts.update(10, listOf(FieldOp.Replace(100, 0, mapOf(StructuredName.GIVEN_NAME to "A")))))
        assertEquals(0, fake.mutations)
    }

    @Test fun invalidPatchesAreRejectedBeforeProviderMutation() {
        assertTrue(contacts.update(10, listOf(FieldOp.Add(Kind.PHONE, mapOf("data9" to "x")))) is WriteResult.Invalid)
        assertTrue(contacts.update(10, listOf(FieldOp.Add(Kind.PHONE, mapOf(Phone.NUMBER to "1", Phone.TYPE to "mobile")))) is WriteResult.Invalid)
        assertTrue(contacts.update(10, listOf(FieldOp.Add(Kind.EVENT, mapOf(Event.START_DATE to "Dec 10")))) is WriteResult.Invalid)
        assertTrue(contacts.update(10, listOf(FieldOp.Add(Kind.NAME, mapOf(StructuredName.GIVEN_NAME to "Second"))) ) is WriteResult.Invalid)
        assertTrue(contacts.update(10, emptyList(), AndroidContactsProvider.PhotoOp.Write(ByteArray(70 * 1024) { 0xFF.toByte() })) is WriteResult.Invalid)
        assertEquals(WriteResult.NotFound, contacts.update(999, listOf(FieldOp.Remove(1, 0))))
        assertEquals(0, fake.mutations)
    }

    @Test fun addAndRemoveTargetOnlyTheRequestedRawContact() {
        val result = contacts.update(11, listOf(FieldOp.Remove(102, 0), FieldOp.Add(Kind.EMAIL, mapOf(Email.ADDRESS to "new@example.test", Email.TYPE to "${Email.TYPE_WORK}"))))
        assertEquals(WriteResult.Applied(11), result)
        assertNull(fake.rows.firstOrNull { it["_id"] == 102L })
        val added = fake.rows.single { it["data1"] == "new@example.test" }
        assertEquals(11L, added["raw_contact_id"])
        assertEquals(Email.CONTENT_ITEM_TYPE, added["mimetype"])
        assertEquals(4, fake.rows.count { it["raw_contact_id"] == 10L }) // other raw contact untouched
    }

    @Test fun createWritesAllCommonFieldsInSelectedWritableAccount() {
        val google = ContactAccount("ada@example.test", GOOGLE)
        val result = contacts.create(google, listOf(
            NewField(Kind.NAME, mapOf(StructuredName.DISPLAY_NAME to "Grace Hopper")),
            NewField(Kind.PHONE, mapOf(Phone.NUMBER to "+12025550100", Phone.TYPE to "${Phone.TYPE_WORK}")),
            NewField(Kind.EMAIL, mapOf(Email.ADDRESS to "grace@example.test")),
            NewField(Kind.NOTE, mapOf(Note.NOTE to "Navy")),
        ))
        val rawId = (result as WriteResult.Applied).rawContactId
        assertEquals(google, fake.raws.single { it["_id"] == rawId }.account())
        assertEquals(setOf(StructuredName.CONTENT_ITEM_TYPE, Phone.CONTENT_ITEM_TYPE, Email.CONTENT_ITEM_TYPE, Note.CONTENT_ITEM_TYPE), fake.rows.filter { it["raw_contact_id"] == rawId }.map { it["mimetype"] }.toSet())
        assertEquals("${Phone.TYPE_WORK}", fake.rows.single { it["raw_contact_id"] == rawId && it["mimetype"] == Phone.CONTENT_ITEM_TYPE }["data2"])
    }

    @Test fun createIntoReadOnlySyncAccountIsRejectedWithoutInsert() {
        assertEquals(WriteResult.ReadOnly, contacts.create(ContactAccount("ada", MESSENGER), listOf(NewField(Kind.NAME, mapOf(StructuredName.DISPLAY_NAME to "X")))))
        assertEquals(0, fake.mutations)
    }

    @Test fun nameOnlyCreateEvidenceAnchorsOnSuppliedNameParts() {
        // Desktop name-only creates carry GIVEN/FAMILY; the provider derives DISPLAY_NAME (DATA1).
        val nameOnly = listOf(NewField(Kind.NAME, mapOf(StructuredName.GIVEN_NAME to "Grace", StructuredName.FAMILY_NAME to "Hopper")))
        assertEquals(emptySet<Long>(), contacts.findCreateMatches(null, nameOnly))
        val created = contacts.create(null, nameOnly) as WriteResult.Applied
        fake.row(fake.rows.single { it["raw_contact_id"] == created.rawContactId }["_id"] as Long)["data1"] = "Grace Hopper"
        assertEquals(setOf(created.rawContactId), contacts.findCreateMatches(null, nameOnly))
    }

    @Test fun createWithoutProviderEvidenceIsOutcomeUnknownAndMatchesStayAmbiguous() {
        val draft = listOf(NewField(Kind.NAME, mapOf(StructuredName.DISPLAY_NAME to "Ada Lovelace")))
        val before = contacts.findCreateMatches(null, listOf(
            NewField(Kind.NAME, mapOf(StructuredName.DISPLAY_NAME to "Ada Lovelace")),
            NewField(Kind.PHONE, mapOf(Phone.NUMBER to "+12025550123")),
            NewField(Kind.EVENT, mapOf(Event.START_DATE to "--12-10")),
        ))
        assertEquals(setOf(10L), before) // preexisting identical local contact is visible evidence
        val beforeNameOnly = contacts.findCreateMatches(null, draft)!!
        assertTrue(beforeNameOnly.isEmpty()) // raw 10 has more fields, so it is not an exact match

        fake.failAfterCommit = true // provider committed, but the caller never saw the result
        assertEquals(WriteResult.OutcomeUnknown, contacts.create(null, draft))
        val after = contacts.findCreateMatches(null, draft)!!
        assertEquals(1, (after - beforeNameOnly).size) // exactly one new candidate: caller may adopt

        fake.failAfterCommit = true
        assertEquals(WriteResult.OutcomeUnknown, contacts.create(null, draft))
        assertEquals(2, (contacts.findCreateMatches(null, draft)!! - beforeNameOnly).size) // ambiguous: must stay OutcomeUnknown
    }

    @Test fun deleteTargetsOneRawContactWithVersionGuard() {
        assertEquals(WriteResult.Conflict, contacts.delete(10, 3))
        assertEquals(WriteResult.Applied(10), contacts.delete(10, 0))
        assertEquals(1, fake.raws.single { it["_id"] == 10L }["deleted"])
        assertEquals(0, fake.raws.single { it["_id"] == 11L }["deleted"])
        assertEquals(WriteResult.NotFound, contacts.delete(10, 0))
    }

    @Test fun observerDebouncesAndCapsLatencyUnderContinuousChanges() {
        var fired = 0
        val observer = ContactChangeObserver(ApplicationProvider.getApplicationContext<android.content.Context>().contentResolver, { fired++ })
        val looper = shadowOf(Looper.getMainLooper())
        observer.onChange(false)
        looper.idleFor(Duration.ofSeconds(44)); assertEquals(0, fired)
        looper.idleFor(Duration.ofSeconds(2)); assertEquals(1, fired)
        repeat(12) { observer.onChange(false); looper.idleFor(Duration.ofSeconds(30)) }
        assertTrue("continuous changes must not starve dispatch", fired >= 2)
    }

    /** Minimal ContactsProvider stand-in honouring the selections the adapter issues. */
    class FakeContactsProvider : ContentProvider() {
        val contacts = mutableListOf<MutableMap<String, Any?>>()
        val raws = mutableListOf<MutableMap<String, Any?>>()
        val rows = mutableListOf<MutableMap<String, Any?>>()
        var mutations = 0
        var nullCursorFor: String? = null
        var denyAccess = false
        var failAfterCommit = false
        var bumpVersionBeforeBatch: Long? = null
        private var nextId = 1000L

        fun contact(id: Long, name: String, lookup: String? = "lookup-$id") { contacts += mutableMapOf("_id" to id, "lookup" to lookup, "display_name" to name, "contact_last_updated_timestamp" to 1L, "photo_id" to null) }
        fun raw(id: Long, contactId: Long, account: ContactAccount?) { raws += mutableMapOf("_id" to id, "contact_id" to contactId, "account_name" to account?.name, "account_type" to account?.type, "raw_contact_is_read_only" to 0, "version" to 0, "deleted" to 0) }
        fun data(id: Long, rawId: Long, mime: String, vararg values: Pair<String, String>, primary: Boolean = false) {
            rows += mutableMapOf<String, Any?>("_id" to id, "raw_contact_id" to rawId, "mimetype" to mime, "is_read_only" to 0, "is_primary" to if (primary) 1 else 0, "data_version" to 0).apply { putAll(values) }
        }
        fun row(id: Long) = rows.single { it["_id"] == id }

        override fun onCreate() = true
        override fun getType(uri: Uri): String? = null

        override fun query(uri: Uri, projection: Array<out String>?, selection: String?, selectionArgs: Array<out String>?, sortOrder: String?): Cursor? {
            if (denyAccess) throw SecurityException("READ_CONTACTS")
            val table = uri.pathSegments.first()
            if (table == nullCursorFor) return null
            val source: List<Map<String, Any?>> = when (table) { "contacts" -> contacts; "raw_contacts" -> raws; "data" -> rows; else -> emptyList() }
            val columns = projection ?: arrayOf("_id")
            val sorted = source.filter { matches(it, selection, selectionArgs) }
                .sortedWith(compareBy<Map<String, Any?>>({ it["raw_contact_id"] as Long? }, { it["_id"] as Long }))
            return MatrixCursor(columns).also { c -> sorted.forEach { row -> c.addRow(columns.map { row[it] }) } }
        }

        override fun applyBatch(operations: ArrayList<ContentProviderOperation>): Array<ContentProviderResult> {
            bumpVersionBeforeBatch?.let { id -> row(id)["data_version"] = (row(id)["data_version"] as Int) + 1; bumpVersionBeforeBatch = null }
            val snapshot = Triple(raws.map { HashMap(it) }, rows.map { HashMap(it) }, mutations)
            val results = try {
                super.applyBatch(operations)
            } catch (e: Exception) { // ContactsProvider applies a batch in one transaction
                raws.clear(); raws.addAll(snapshot.first); rows.clear(); rows.addAll(snapshot.second); mutations = snapshot.third
                throw e
            }
            if (failAfterCommit) { failAfterCommit = false; throw IllegalStateException("binder died after commit") }
            return results
        }

        override fun insert(uri: Uri, values: ContentValues?): Uri? {
            mutations++
            val id = nextId++
            val map = mutableMapOf<String, Any?>("_id" to id)
            values?.keySet()?.forEach { map[it] = values.get(it) }
            return when (uri.pathSegments.first()) {
                "raw_contacts" -> {
                    raws += map.apply { putIfAbsent("account_name", null); putIfAbsent("account_type", null); put("contact_id", id); put("raw_contact_is_read_only", 0); put("version", 0); put("deleted", 0) }
                    contact(id, "created")
                    Uri.withAppendedPath(uri, "$id")
                }
                else -> {
                    rows += map.apply { put("raw_contact_id", (get("raw_contact_id") as Number).toLong()); put("is_read_only", 0); put("is_primary", 0); put("data_version", 0) }
                    Uri.withAppendedPath(uri, "$id")
                }
            }
        }

        override fun update(uri: Uri, values: ContentValues?, selection: String?, selectionArgs: Array<out String>?): Int {
            val hits = rows.filter { matches(it, selection, selectionArgs) }
            hits.forEach { row -> mutations++; values?.keySet()?.forEach { row[it] = values.get(it) }; row["data_version"] = (row["data_version"] as Int) + 1 }
            return hits.size
        }

        override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?): Int = when (uri.pathSegments.first()) {
            "raw_contacts" -> {
                val id = uri.lastPathSegment!!.toLong()
                val hits = raws.filter { it["_id"] == id && it["deleted"] == 0 && matches(it, selection, selectionArgs) }
                hits.forEach { mutations++; it["deleted"] = 1 }
                hits.size
            }
            else -> {
                val hits = rows.filter { matches(it, selection, selectionArgs) }
                mutations += hits.size; rows.removeAll(hits); hits.size
            }
        }

        /** Supports `col=?`, `col=0`, `col > ?` and `col IN (?,…)` clauses joined by AND. */
        private fun matches(row: Map<String, Any?>, selection: String?, args: Array<out String>?): Boolean {
            if (selection == null) return true
            var arg = 0
            return selection.split(" AND ").all { clause ->
                val inMatch = Regex("""(\w+) IN \(([?,]+)\)""").matchEntire(clause.trim())
                when {
                    inMatch != null -> {
                        val n = inMatch.groupValues[2].count { it == '?' }
                        val wanted = args!!.slice(arg until arg + n).toSet(); arg += n
                        row[inMatch.groupValues[1]]?.toString() in wanted
                    }
                    clause.contains(" > ?") -> (row[clause.substringBefore(" >").trim()] as Long) > args!![arg++].toLong()
                    clause.endsWith("=?") -> row[clause.substringBefore("=").trim()]?.toString() == args!![arg++]
                    else -> row[clause.substringBefore("=").trim()]?.toString() == clause.substringAfter("=").trim()
                }
            }
        }
    }

    private fun Map<String, Any?>.account(): ContactAccount? =
        (this["account_name"] as String?)?.let { ContactAccount(it, this["account_type"] as String) }

    private companion object {
        const val GOOGLE = "com.google"
        const val MESSENGER = "com.example.messenger"
    }
}
