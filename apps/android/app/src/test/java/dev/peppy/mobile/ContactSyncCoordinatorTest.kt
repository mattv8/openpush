package dev.peppy.mobile

import android.provider.ContactsContract.CommonDataKinds.Phone
import android.provider.ContactsContract.CommonDataKinds.StructuredName
import dev.peppy.mobile.AndroidContactsProvider.Contact
import dev.peppy.mobile.AndroidContactsProvider.Failure
import dev.peppy.mobile.AndroidContactsProvider.Field
import dev.peppy.mobile.AndroidContactsProvider.FieldOp
import dev.peppy.mobile.AndroidContactsProvider.Kind
import dev.peppy.mobile.AndroidContactsProvider.Lookup
import dev.peppy.mobile.AndroidContactsProvider.NewField
import dev.peppy.mobile.AndroidContactsProvider.Page
import dev.peppy.mobile.AndroidContactsProvider.PhotoOp
import dev.peppy.mobile.AndroidContactsProvider.PhotoRead
import dev.peppy.mobile.AndroidContactsProvider.RawSource
import dev.peppy.mobile.AndroidContactsProvider.WriteResult
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import uniffi.peppy_mobile_bindings.NativeClient
import java.util.UUID

/**
 * Coordinator against the REAL generated bindings and SQLCipher core (no fake facade) with an
 * in-memory OS contacts provider. Desktop-originated edit requests come from a second real core
 * of the same vault and are delivered exactly as HTTP replay would. No real contacts are touched.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class ContactSyncCoordinatorTest : GatewayTestBase() {
    private var cursor = 0
    private val os = FakeContactsOs()
    private var access = ContactsAccess(read = true, write = true)

    private fun coordinator(client: NativeClient) = ContactSyncCoordinator(
        NativeContactCore(client), os, deviceId, { access }, java.io.File(context.cacheDir, "photos"),
        deviceName = "Pixel Test", region = "us",
    )

    private fun view(client: NativeClient, bookId: String): JSONArray =
        JSONObject(client.contactBookView(JSONObject().put("book_id", bookId).toString())).getJSONArray("contacts")

    private fun request(client: NativeClient, id: String): JSONObject? {
        val rows = JSONObject(client.listContactRequestsJson("{}")).getJSONArray("requests")
        return (0 until rows.length()).map { rows.getJSONObject(it) }.firstOrNull { it.optString("request_id") == id }
    }

    /** Desktop core authors the request; the gateway ingests and applies it like live replay. */
    private fun deliverFromDesktop(gateway: NativeClient, request: JSONObject) {
        val desktop = SharedVault.desktop
        desktop.setServerCompactionState(true, true)
        desktop.requestContactEdit(request.toString())
        for (wire in desktop.pendingOutboxJson()) {
            gateway.ingestRaw(wire.toByteArray(), (++cursor).toString())
            desktop.ackOutbox(JSONObject(wire).getString("envelope_id"))
        }
        gateway.applyPending(50uL)
    }

    private fun setUpBook(): Pair<NativeClient, String> {
        val gateway = enrollAndUnlock()
        gateway.setServerCompactionState(true, true)
        os.add(1, "Ada Lovelace", "+12025550100")
        os.add(2, "Grace Hopper", "+12025550101")
        val report = coordinator(gateway).run(charging = false)
        assertTrue(report.scanComplete)
        return gateway to checkNotNull(coordinator(gateway).ownedBook()).id
    }

    @Test fun firstPassScansAndCapturesCommonFieldsWithCoreIds() {
        val (gateway, bookId) = setUpBook()
        val contacts = view(gateway, bookId)
        assertEquals(2, contacts.length())
        val ada = (0 until 2).map { contacts.getJSONObject(it) }.single { it.getString("display_name") == "Ada Lovelace" }
        assertEquals("Ada", ada.getJSONObject("name").getString("given"))
        val phone = ada.getJSONArray("phones").getJSONObject(0)
        assertEquals("+12025550100", phone.getString("value"))
        assertEquals("mobile", phone.getString("label"))
        // Core minted its own item ID; provider Data._IDs never reach the UI DTO.
        assertFalse(phone.getString("id") == "101")
        val book = JSONObject(gateway.listContactBooksJson()).getJSONArray("books").getJSONObject(0)
        assertEquals("auto", book.getString("effective_remote_edits"))
        assertEquals("Pixel Test", book.getString("device_name"))
        assertEquals("US", book.getString("region"))
        assertFalse(book.toString().contains("android:local")) // account IDs stay native-only
    }

    @Test fun unchangedIncrementalPassMintsNothing() {
        val (gateway, bookId) = setUpBook()
        val before = view(gateway, bookId).toString()
        val outbox = gateway.pendingOutboxJson().size
        val report = coordinator(gateway).run(charging = false)
        assertFalse(report.scanFinished)
        assertEquals(before, view(gateway, bookId).toString())
        assertEquals(outbox, gateway.pendingOutboxJson().size)
    }

    @Test fun revokedOrFailedScansNeverDeleteAndCompleteScanDoes() {
        val (gateway, bookId) = setUpBook()
        os.remove(2)
        os.failScan = Failure.PERMISSION_DENIED
        val failed = coordinator(gateway).run(charging = false, forceFull = true)
        assertFalse(failed.scanComplete)
        assertEquals(2, view(gateway, bookId).length())

        os.failScan = null
        access = ContactsAccess(read = false, write = false)
        assertEquals(ContactPassReport.Outcome.NO_ACCESS, coordinator(gateway).run(charging = false).outcome)
        assertEquals(2, view(gateway, bookId).length())
        assertEquals("unavailable", JSONObject(gateway.listContactBooksJson()).getJSONArray("books").getJSONObject(0).getString("state"))

        access = ContactsAccess(read = true, write = true)
        val complete = coordinator(gateway).run(charging = false, forceFull = true)
        assertTrue(complete.scanComplete)
        assertEquals(1, complete.deleted)
        assertEquals(1, view(gateway, bookId).length())
        assertTrue(os.writes.isEmpty()) // projection-only deletion, no OS effect
    }

    @Test fun desktopUpdateIsPermittedWrittenThenReconciledFromObservedState() {
        val (gateway, bookId) = setUpBook()
        val ada = (0 until 2).map { view(gateway, bookId).getJSONObject(it) }.single { it.getString("display_name") == "Ada Lovelace" }
        val phoneId = ada.getJSONArray("phones").getJSONObject(0).getString("id")
        val requestId = UUID.randomUUID().toString()
        deliverFromDesktop(gateway, JSONObject().put("schema_version", 1).put("request_id", requestId).put("target_owner", deviceId)
            .put("book_id", bookId).put("kind", "update").put("contact_id", ada.getString("id")).put("base_revision", ada.getString("revision"))
            .put("patches", JSONArray().put(JSONObject().put("op", "replace").put("path", "phones[$phoneId]")
                .put("value", JSONObject().put("id", phoneId).put("value", "+12025550199").put("label", "work")))))
        assertTrue(os.writes.isEmpty()) // historical/received requests never write by themselves

        val report = coordinator(gateway).run(charging = false)

        assertEquals(1, report.applied)
        assertEquals(listOf("update:10"), os.writes)
        val phone = os.contacts.getValue(1).fields.single { it.kind == Kind.PHONE }
        assertEquals("+12025550199", phone.values[Phone.NUMBER])
        assertEquals("${Phone.TYPE_WORK}", phone.values[Phone.TYPE])
        assertEquals("applied", request(gateway, requestId)!!.getString("state"))
        val observed = (0 until 2).map { view(gateway, bookId).getJSONObject(it) }.single { it.getString("id") == ada.getString("id") }
        val observedPhone = observed.getJSONArray("phones").getJSONObject(0)
        assertEquals("+12025550199", observedPhone.getString("value"))
        assertEquals(phoneId, observedPhone.getString("id")) // item identity preserved

        // A second pass never re-applies a terminal request.
        coordinator(gateway).run(charging = false)
        assertEquals(1, os.writes.size)
    }

    @Test fun confirmPolicyWaitsForLocalApprovalBeforeAnyWrite() {
        val (gateway, bookId) = setUpBook()
        coordinator(gateway).setRemoteEdits("confirm")
        assertEquals("confirm", coordinator(gateway).overview().remoteEdits)
        val ada = (0 until 2).map { view(gateway, bookId).getJSONObject(it) }.single { it.getString("display_name") == "Ada Lovelace" }
        val requestId = UUID.randomUUID().toString()
        deliverFromDesktop(gateway, JSONObject().put("schema_version", 1).put("request_id", requestId).put("target_owner", deviceId)
            .put("book_id", bookId).put("kind", "update").put("contact_id", ada.getString("id")).put("base_revision", ada.getString("revision"))
            .put("patches", JSONArray().put(JSONObject().put("op", "replace").put("path", "name.given").put("value", "Augusta"))))

        assertEquals(1, coordinator(gateway).run(charging = false).awaitingApproval)
        assertTrue(os.writes.isEmpty())
        val pending = coordinator(gateway).overview().pending.single { it.id == requestId }
        coordinator(gateway).decide(pending, approve = true)

        assertEquals(1, coordinator(gateway).run(charging = false).applied)
        assertEquals("Augusta", os.contacts.getValue(1).fields.single { it.kind == Kind.NAME }.values[StructuredName.GIVEN_NAME])
    }

    private fun createRequest(bookId: String) = JSONObject().put("schema_version", 1).put("request_id", UUID.randomUUID().toString())
        .put("target_owner", deviceId).put("book_id", bookId).put("kind", "create").put("display_name", "Katherine Johnson")
        .put("phones", JSONArray().put(JSONObject().put("id", "n1").put("value", "+12025550142").put("label", "mobile")))

    @Test fun uncertainCreateIsNeverRetriedAndAdoptsOnlyAUniqueNewContact() {
        val (gateway, bookId) = setUpBook()
        val create = createRequest(bookId)
        deliverFromDesktop(gateway, create)
        os.createMode = FakeContactsOs.CreateMode.COMMITTED_BUT_UNKNOWN

        assertEquals(1, coordinator(gateway).run(charging = false).outcomeUnknown)
        assertEquals("outcome_unknown", request(gateway, create.getString("request_id"))!!.getString("state"))
        assertEquals(1, os.creates)

        val recovered = coordinator(gateway).run(charging = false)
        assertEquals(1, os.creates) // evidence-based adoption, no second create
        assertEquals(1, recovered.applied)
        assertEquals("applied", request(gateway, create.getString("request_id"))!!.getString("state"))
    }

    @Test fun uncertainCreateWithoutEvidenceStaysUnknownUntilLocallyResolved() {
        val (gateway, bookId) = setUpBook()
        val create = createRequest(bookId)
        deliverFromDesktop(gateway, create)
        os.createMode = FakeContactsOs.CreateMode.NOT_COMMITTED_UNKNOWN
        coordinator(gateway).run(charging = false)
        coordinator(gateway).run(charging = false)
        assertEquals(1, os.creates)
        val id = create.getString("request_id")
        assertEquals("outcome_unknown", request(gateway, id)!!.getString("state"))
        assertTrue(coordinator(gateway).overview().pending.any { it.id == id && it.state == "outcome_unknown" })

        coordinator(gateway).resolveUnknown(id)
        assertEquals("failed", request(gateway, id)!!.getString("state"))
        assertEquals(1, os.creates)
    }

    @Test fun writeWithoutWritePermissionIsRefusedBeforeAnyOsEffect() {
        val (gateway, bookId) = setUpBook()
        access = ContactsAccess(read = true, write = false)
        coordinator(gateway).run(charging = false) // publishes write:false
        val create = createRequest(bookId)
        deliverFromDesktop(gateway, create)
        coordinator(gateway).run(charging = false)
        assertEquals(0, os.creates)
        assertEquals("rejected", request(gateway, create.getString("request_id"))!!.getString("state"))
    }

    @Test fun retirePublishesRetiredBookWithoutTouchingOsContacts() {
        val (gateway, _) = setUpBook()
        assertTrue(coordinator(gateway).retire())
        val book = JSONObject(gateway.listContactBooksJson()).getJSONArray("books").getJSONObject(0)
        assertEquals("retired", book.getString("state"))
        assertNull(coordinator(gateway).ownedBook())
        assertTrue(os.writes.isEmpty())
        assertEquals(2, os.contacts.size)
    }

    @Test fun hostPassHonoursConsentAndServerCompatibility() {
        val gateway = enrollAndUnlock()
        grant(android.Manifest.permission.READ_CONTACTS, android.Manifest.permission.WRITE_CONTACTS)
        val session = checkNotNull(NativeGateway.session(context))
        ContactSyncPreferences(context).enabled = false
        assertFalse(ContactSyncHost.pass(context, session))
        ContactSyncPreferences(context).enabled = true
        gateway.setServerCompactionSupported(false)
        assertFalse(ContactSyncHost.pass(context, session))
        assertEquals(0, JSONObject(gateway.listContactBooksJson()).getJSONArray("books").length())
        ContactSyncPreferences(context).enabled = false
        ContactSyncHost.ensureObserver(context)
    }


    private fun contactNamed(client: NativeClient, bookId: String, name: String): JSONObject {
        val contacts = view(client, bookId)
        return (0 until contacts.length()).map { contacts.getJSONObject(it) }.single { it.getString("display_name") == name }
    }

    private fun deleteRequest(bookId: String, contact: JSONObject) = JSONObject().put("schema_version", 1)
        .put("request_id", UUID.randomUUID().toString()).put("target_owner", deviceId).put("book_id", bookId).put("kind", "delete")
        .put("contact_id", contact.getString("id")).put("base_revision", contact.getString("revision"))

    private fun deliverApprovedDelete(gateway: NativeClient, request: JSONObject) {
        deliverFromDesktop(gateway, request)
        val id = request.getString("request_id")
        // One deletion exceeds 5% of these deliberately tiny fixture books. Exercise the
        // owner approval path before testing the native write's independent safety checks.
        val pending = JSONObject(gateway.nextContactApplyPermit(JSONObject().put("request_id", id).toString()))
        assertEquals("awaiting_approval", pending.getString("status"))
        gateway.contactApprovalJson(JSONObject().put("request_id", id).put("approve", true).toString())
    }

    @Test fun renameChangesLookupKeyButKeepsCoreIdentityWithoutDuplicate() {
        val (gateway, bookId) = setUpBook()
        val ada = contactNamed(gateway, bookId, "Ada Lovelace")
        val oldKey = os.lookupKey(1)
        os.rename(1, "Ada King")
        assertFalse(oldKey == os.lookupKey(1))

        coordinator(gateway).run(charging = false)

        assertEquals(2, view(gateway, bookId).length())
        assertEquals(ada.getString("id"), contactNamed(gateway, bookId, "Ada King").getString("id"))
    }

    @Test fun deleteAfterPhoneSideJoinIsStaleAndNeverTouchesTheJoinedRaw() {
        val (gateway, bookId) = setUpBook()
        val ada = contactNamed(gateway, bookId, "Ada Lovelace")
        os.join(into = 1, other = 2) // Grace's raw 20 now aggregates with Ada's raw 10
        val request = deleteRequest(bookId, ada)
        deliverApprovedDelete(gateway, request)

        val report = coordinator(gateway).run(charging = false)

        assertEquals(1, report.failed)
        assertTrue(os.writes.isEmpty())
        assertTrue(os.raws.containsKey(10) && os.raws.containsKey(20))
        assertEquals("failed", request(gateway, request.getString("request_id"))!!.getString("state"))
        // The stale refusal schedules a full scan.
        assertTrue(coordinator(gateway).run(charging = false).scanFinished)
    }

    @Test fun updateAfterPhoneSideSplitIsStaleWithoutWrites() {
        val (gateway, bookId) = setUpBook()
        os.join(into = 1, other = 2, visible = true)
        coordinator(gateway).run(charging = false, forceFull = true) // Ada now captured as raws {10, 20}
        val ada = contactNamed(gateway, bookId, "Ada Lovelace")
        os.split(20)
        val request = JSONObject().put("schema_version", 1).put("request_id", UUID.randomUUID().toString()).put("target_owner", deviceId)
            .put("book_id", bookId).put("kind", "update").put("contact_id", ada.getString("id")).put("base_revision", ada.getString("revision"))
            .put("patches", JSONArray().put(JSONObject().put("op", "replace").put("path", "nickname").put("value", "Countess")))
        deliverFromDesktop(gateway, request)

        coordinator(gateway).run(charging = false)

        assertTrue(os.writes.isEmpty())
        // Refused natively (stale_item) or, if a rescan saw the split first, by the core's base check.
        assertTrue(request(gateway, request.getString("request_id"))!!.getString("state") in setOf("failed", "conflict"))
    }

    @Test fun createAutoJoinedIntoAnExistingContactNeverClaimsItsRaws() {
        val (gateway, bookId) = setUpBook()
        val ada = contactNamed(gateway, bookId, "Ada Lovelace")
        os.autoJoinInto = 1
        val create = JSONObject().put("schema_version", 1).put("request_id", UUID.randomUUID().toString()).put("target_owner", deviceId)
            .put("book_id", bookId).put("kind", "create").put("display_name", "Ada Lovelace")
            .put("emails", JSONArray().put(JSONObject().put("id", "e1").put("value", "ada@example.test")))
        deliverFromDesktop(gateway, create)

        assertEquals(1, coordinator(gateway).run(charging = false).applied)
        val adas = (0 until view(gateway, bookId).length()).map { view(gateway, bookId).getJSONObject(it) }.filter { it.getString("display_name") == "Ada Lovelace" }
        assertEquals(2, adas.size)
        val created = adas.single { it.getString("id") != ada.getString("id") }
        assertFalse(created.has("phones")) // only the created raw's fields, not Ada's phone
        assertEquals(ada.getString("revision"), adas.single { it.getString("id") == ada.getString("id") }.getString("revision"))

        // Deleting the created record cannot delete pre-existing raw 10 through the joined aggregate.
        os.autoJoinInto = null
        deliverApprovedDelete(gateway, deleteRequest(bookId, created))
        coordinator(gateway).run(charging = false)
        assertTrue(os.raws.containsKey(10))
        assertTrue(os.writes.none { it.startsWith("delete") })
    }

    @Test fun phoneSideEditBeforePermitWinsOverAStaleLabelPatch() {
        val (gateway, bookId) = setUpBook()
        val ada = contactNamed(gateway, bookId, "Ada Lovelace")
        val phone = ada.getJSONArray("phones").getJSONObject(0)
        os.edit(1, Kind.PHONE, mapOf(Phone.NUMBER to "+19995550000")) // after capture, before permit, not yet rescanned
        val request = JSONObject().put("schema_version", 1).put("request_id", UUID.randomUUID().toString()).put("target_owner", deviceId)
            .put("book_id", bookId).put("kind", "update").put("contact_id", ada.getString("id")).put("base_revision", ada.getString("revision"))
            .put("patches", JSONArray().put(JSONObject().put("op", "replace").put("path", "phones[${phone.getString("id")}]")
                .put("value", JSONObject(phone.toString()).put("label", "work"))))
        deliverFromDesktop(gateway, request)

        coordinator(gateway).run(charging = false)

        assertTrue(os.writes.isEmpty())
        assertEquals("+19995550000", os.contacts.getValue(1).fields.single { it.kind == Kind.PHONE }.values[Phone.NUMBER])
        assertEquals("failed", request(gateway, request.getString("request_id"))!!.getString("state"))
        // The fresh phone value was published as the observed state.
        assertEquals("+19995550000", contactNamed(gateway, bookId, "Ada Lovelace").getJSONArray("phones").getJSONObject(0).getString("value"))
    }

    @Test fun deleteOfAContactEditedOnThePhoneSinceCaptureIsRefused() {
        val (gateway, bookId) = setUpBook()
        val grace = contactNamed(gateway, bookId, "Grace Hopper")
        os.edit(2, Kind.PHONE, mapOf(Phone.NUMBER to "+19995550001"))
        val request = deleteRequest(bookId, grace)
        deliverApprovedDelete(gateway, request)

        coordinator(gateway).run(charging = false)

        assertTrue(os.raws.containsKey(20))
        assertTrue(os.writes.isEmpty())
        assertEquals("failed", request(gateway, request.getString("request_id"))!!.getString("state"))
    }

    @Test fun photoOnlyEditDoesNotRemoveANewUncapturedPhonePhoto() {
        val (gateway, bookId) = setUpBook()
        val ada = contactNamed(gateway, bookId, "Ada Lovelace")
        // The phone changed after capture, outside this pass's incremental scan.
        os.raws.getValue(10).fields += Field(
            102, 10, Kind.PHOTO,
            mapOf(android.provider.ContactsContract.CommonDataKinds.Photo.PHOTO_FILE_ID to "77"),
            false, true, 1,
        )
        val edit = JSONObject().put("schema_version", 1).put("request_id", UUID.randomUUID().toString())
            .put("target_owner", deviceId).put("book_id", bookId).put("kind", "update")
            .put("contact_id", ada.getString("id")).put("base_revision", ada.getString("revision"))
            .put("patches", JSONArray()).put("photo_op", JSONObject().put("op", "remove"))
        deliverFromDesktop(gateway, edit)

        coordinator(gateway).run(charging = false)

        assertTrue("A newer phone photo must not be overwritten", os.writes.isEmpty())
        assertEquals("failed", request(gateway, edit.getString("request_id"))!!.getString("state"))
    }

    @Test fun unchangedDeleteRemovesOnlyTheCapturedRaws() {
        val (gateway, bookId) = setUpBook()
        val request = deleteRequest(bookId, contactNamed(gateway, bookId, "Grace Hopper"))
        deliverApprovedDelete(gateway, request)
        coordinator(gateway).run(charging = false)
        assertEquals(listOf("delete:20"), os.writes)
        assertEquals("applied", request(gateway, request.getString("request_id"))!!.getString("state"))
        assertTrue(os.raws.containsKey(10))
    }

    @Test fun overviewExposesCoreAccountsWithoutProviderEmailInIds() {
        os.accountList += ContactAccount("ada@example.test", "com.google")
        val (gateway, _) = setUpBook()
        val overview = coordinator(gateway).overview()
        assertEquals(setOf(ContactMapping.LOCAL_ACCOUNT_ID, ContactMapping.accountId(ContactAccount("ada@example.test", "com.google"))), overview.accounts.map { it.id }.toSet())
        assertTrue(overview.accounts.none { it.id.contains("@") })
        assertNotNull(overview.lastFullScanAt)
        val google = overview.accounts.single { it.id != ContactMapping.LOCAL_ACCOUNT_ID }
        coordinator(gateway).setDefaultAccount(google.id)
        assertEquals(google.id, coordinator(gateway).overview().defaultAccountId)
    }
}

/**
 * In-memory OS contacts modelled like ContactsProvider: raw contacts aggregated into contacts,
 * an AOSP-like LOOKUP_KEY (`r<minRaw>-<display name>`, so renames change it), joins, splits and
 * auto-aggregation of new raws. Initial raw id = contact id * 10.
 */
internal class FakeContactsOs : ContactsOs {
    enum class CreateMode { APPLY, COMMITTED_BUT_UNKNOWN, NOT_COMMITTED_UNKNOWN }

    class Raw(var contactId: Long, val account: ContactAccount?, var version: Int, val fields: MutableList<Field>)

    val raws = sortedMapOf<Long, Raw>()
    val writes = mutableListOf<String>()
    val accountList = mutableListOf<ContactAccount>()
    var failScan: Failure? = null
    var createMode = CreateMode.APPLY
    /** When set, a created raw is auto-joined into this existing contact. */
    var autoJoinInto: Long? = null
    var creates = 0
    private val updated = mutableMapOf<Long, Long>()
    private var nextData = 1000L
    private var nextContact = 100L
    private var nextRaw = 5000L

    /** Current aggregates, keyed by contact id. */
    val contacts: Map<Long, Contact> get() = raws.values.map { it.contactId }.distinct().associateWith { aggregate(it)!! }.toSortedMap()

    fun lookupKey(contactId: Long) = aggregate(contactId)?.lookupKey

    private fun aggregate(contactId: Long): Contact? {
        val members = raws.filterValues { it.contactId == contactId }
        if (members.isEmpty()) return null
        val minRaw = members.keys.min()
        val name = members.getValue(minRaw).fields.firstOrNull { it.kind == Kind.NAME }?.values?.get(StructuredName.DISPLAY_NAME).orEmpty()
        val fields = members.values.flatMap { it.fields }
        return Contact(
            "r$minRaw-$name", contactId, name, updated[contactId] ?: 1L, fields.firstOrNull { it.kind == Kind.PHOTO }?.dataId,
            members.map { (id, raw) -> RawSource(id, contactId, raw.account, true, raw.version) }, fields, false,
        )
    }

    private fun touch(contactId: Long) { updated[contactId] = System.currentTimeMillis() + 3_600_000 }

    fun add(id: Long, name: String, phone: String) {
        val raw = id * 10
        val (given, family) = name.split(" ").let { it.first() to it.drop(1).joinToString(" ") }
        raws[raw] = Raw(id, null, 0, mutableListOf(
            Field(id * 100, raw, Kind.NAME, mapOf(StructuredName.DISPLAY_NAME to name, StructuredName.GIVEN_NAME to given, StructuredName.FAMILY_NAME to family), false, true, 0),
            Field(id * 100 + 1, raw, Kind.PHONE, mapOf(Phone.NUMBER to phone, Phone.TYPE to "${Phone.TYPE_MOBILE}"), false, true, 0),
        ))
    }

    fun remove(id: Long) { raws.entries.removeIf { it.value.contactId == id } }

    /** Phone-side edit of a field (bumps the row's DATA_VERSION); [visible] updates the timestamp. */
    fun edit(contactId: Long, kind: Kind, values: Map<String, String?>, visible: Boolean = false) {
        val raw = raws.values.first { it.contactId == contactId && it.fields.any { f -> f.kind == kind } }
        val index = raw.fields.indexOfFirst { it.kind == kind }
        raw.fields[index] = raw.fields[index].let { it.copy(values = it.values + values, version = it.version + 1) }
        raw.version++
        if (visible) touch(contactId)
    }

    fun rename(contactId: Long, name: String) {
        edit(contactId, Kind.NAME, mapOf(StructuredName.DISPLAY_NAME to name, StructuredName.GIVEN_NAME to name.substringBefore(' '), StructuredName.FAMILY_NAME to name.substringAfter(' ', "")), visible = true)
    }

    /** Joins [other]'s raw contacts into [into] (no timestamp change unless [visible]). */
    fun join(into: Long, other: Long, visible: Boolean = false) {
        raws.values.filter { it.contactId == other }.forEach { it.contactId = into }
        if (visible) touch(into)
    }

    /** Splits [rawId] out of its aggregate into a new contact. */
    fun split(rawId: Long): Long {
        val id = nextContact++
        raws.getValue(rawId).contactId = id
        return id
    }

    override fun accounts() = accountList.toList()
    override fun osDefaultAccount() = AndroidContactsProvider.DefaultAccount.Unavailable
    override fun scan(afterContactId: Long?, limit: Int, updatedSince: Long?): Page {
        failScan?.let { return Page.Failed(it) }
        val rows = contacts.values.filter { it.contactId > (afterContactId ?: 0L) && (updatedSince == null || it.updatedAt > updatedSince) }
        val page = rows.take(limit)
        return Page.Ready(page, page.lastOrNull()?.contactId ?: afterContactId, rows.size > limit, false)
    }
    override fun contactByLookup(lookupKey: String): Lookup =
        contacts.values.firstOrNull { it.lookupKey == lookupKey }?.let { Lookup.Found(it) } ?: Lookup.Missing
    override fun contactByRawContact(rawContactId: Long): Lookup =
        raws[rawContactId]?.let { Lookup.Found(aggregate(it.contactId)!!) } ?: Lookup.Missing
    override fun deletedSince(sinceMillis: Long) = 0
    override fun readPhoto(contactId: Long): PhotoRead = PhotoRead.None

    override fun update(rawContactId: Long, ops: List<FieldOp>, photo: PhotoOp?): WriteResult {
        val raw = raws[rawContactId] ?: return WriteResult.NotFound
        writes += "update:$rawContactId"
        for (op in ops) {
            when (op) {
                is FieldOp.Replace -> {
                    val index = raw.fields.indexOfFirst { it.dataId == op.dataId }
                    if (index < 0 || raw.fields[index].version != op.expectedVersion) return WriteResult.Conflict
                    val row = raw.fields[index]
                    var values = row.values + op.values
                    if (row.kind == Kind.NAME && StructuredName.DISPLAY_NAME !in op.values) {
                        values = values + (StructuredName.DISPLAY_NAME to listOfNotNull(values[StructuredName.GIVEN_NAME], values[StructuredName.FAMILY_NAME]).joinToString(" "))
                    }
                    raw.fields[index] = row.copy(values = values, version = row.version + 1)
                }
                is FieldOp.Remove -> raw.fields.removeIf { it.dataId == op.dataId }
                is FieldOp.Add -> raw.fields += Field(nextData++, rawContactId, op.kind, op.values, false, false, 0)
            }
        }
        raw.version++
        return WriteResult.Applied(rawContactId)
    }

    override fun create(account: ContactAccount?, fields: List<NewField>, photo: ByteArray?): WriteResult {
        creates++
        writes += "create"
        if (createMode == CreateMode.NOT_COMMITTED_UNKNOWN) return WriteResult.OutcomeUnknown
        val raw = nextRaw++
        val contactId = autoJoinInto ?: nextContact++
        raws[raw] = Raw(contactId, account, 0, fields.map { Field(nextData++, raw, it.kind, it.values, false, false, 0) }.toMutableList())
        return if (createMode == CreateMode.COMMITTED_BUT_UNKNOWN) WriteResult.OutcomeUnknown else WriteResult.Applied(raw)
    }

    override fun delete(rawContactId: Long, expectedVersion: Int): WriteResult {
        writes += "delete:$rawContactId"
        val raw = raws[rawContactId] ?: return WriteResult.NotFound
        if (raw.version != expectedVersion) return WriteResult.Conflict
        raws.remove(rawContactId)
        return WriteResult.Applied(rawContactId)
    }

    override fun findCreateMatches(account: ContactAccount?, fields: List<NewField>): Set<Long> {
        val name = fields.firstOrNull { it.kind == Kind.NAME }?.values?.get(StructuredName.DISPLAY_NAME) ?: return emptySet()
        return raws.filter { (_, raw) -> raw.account == account && raw.fields.any { it.kind == Kind.NAME && it.values[StructuredName.DISPLAY_NAME] == name } }.keys
    }
}
