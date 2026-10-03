package dev.openpush.mobile

import android.provider.ContactsContract.CommonDataKinds.Event
import android.provider.ContactsContract.CommonDataKinds.Phone
import android.provider.ContactsContract.CommonDataKinds.StructuredName
import dev.openpush.mobile.AndroidContactsProvider.Contact
import dev.openpush.mobile.AndroidContactsProvider.Field
import dev.openpush.mobile.AndroidContactsProvider.FieldOp
import dev.openpush.mobile.AndroidContactsProvider.Kind
import dev.openpush.mobile.AndroidContactsProvider.RawSource
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** Pure provider <-> core mapping and patch planning; no core, no OS provider. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class ContactMappingTest {
    private val google = ContactAccount("ada@example.test", "com.google")
    private val messenger = ContactAccount("ada", "com.example.messenger")

    private fun contact() = Contact(
        "lookup-1", 1, "Ada Lovelace", 1L, null,
        listOf(RawSource(10, 1, google, true, 3), RawSource(11, 1, messenger, false, 0)),
        listOf(
            Field(100, 10, Kind.NAME, mapOf(StructuredName.DISPLAY_NAME to "Ada Lovelace", StructuredName.GIVEN_NAME to "Ada", StructuredName.FAMILY_NAME to "Lovelace"), false, true, 4),
            Field(101, 10, Kind.PHONE, mapOf(Phone.NUMBER to "+12025550100", Phone.TYPE to "${Phone.TYPE_MOBILE}"), false, true, 2),
            Field(102, 11, Kind.PHONE, mapOf(Phone.NUMBER to "+12025550999", Phone.TYPE to "0", Phone.LABEL to "Messenger"), true, false, 0),
            Field(103, 11, Kind.PHONE, mapOf(Phone.NUMBER to "+12025550100", Phone.TYPE to "${Phone.TYPE_MOBILE}"), true, false, 0),
            Field(104, 10, Kind.EVENT, mapOf(Event.START_DATE to "--12-10", Event.TYPE to "${Event.TYPE_BIRTHDAY}"), false, false, 0),
        ),
        false,
    )

    @Test fun fieldsCarryProvenanceTokensAndDedupeAcrossRawContacts() {
        val entry = ContactMapping.entry(contact(), ContactMapping.accountId(google))
        val fields = entry.getJSONObject("fields")
        assertEquals("Ada", fields.getJSONObject("name").getString("given"))
        val phones = fields.getJSONArray("phones")
        assertEquals(2, phones.length()) // the duplicate read-only copy is folded into the writable one
        assertEquals("101", phones.getJSONObject(0).getString("id"))
        assertEquals("mobile", phones.getJSONObject(0).getString("label"))
        assertTrue(phones.getJSONObject(0).getBoolean("writable"))
        assertEquals("Messenger", phones.getJSONObject(1).getString("label"))
        assertFalse(phones.getJSONObject(1).getBoolean("writable"))
        assertEquals(JSONObject().put("month", 12).put("day", 10).toString(), fields.getJSONObject("birthday").toString())
        val provenance = entry.getJSONObject("provenance")
        assertEquals(ContactMapping.accountId(google), provenance.getString("account_id"))
        assertFalse(provenance.getBoolean("read_only"))
        // Identity is the raw set, never the (rename-sensitive) lookup key, which is only a hint.
        assertEquals("raw:10", entry.getString("source_key"))
        assertEquals(listOf("10", "11"), (0 until 2).map { provenance.getJSONArray("raw_ids").getString(it) })
        assertEquals("lookup-1", provenance.getString("lookup_hint"))
        assertFalse(ContactMapping.accountId(google).contains("@"))
        // Text capture omits `photo` (core inherits the stored photo) unless told otherwise.
        assertFalse(fields.has("photo"))
        val cleared = ContactMapping.entry(contact(), null, ContactMapping.PhotoField.Clear, "fp-1")
        assertTrue(cleared.getJSONObject("fields").isNull("photo"))
        assertEquals("fp-1", cleared.getJSONObject("provenance").getString("photo_source_hash"))
        val set = ContactMapping.entry(contact(), null, ContactMapping.PhotoField.Set("a1"))
        assertEquals("a1", set.getJSONObject("fields").getJSONObject("photo").getString("attachment_id"))
    }

    @Test fun labelsAndDatesRoundTrip() {
        assertEquals("${Phone.TYPE_WORK}" to null, ContactMapping.typeAndLabel(Kind.PHONE, "work"))
        assertEquals("0" to "Boat", ContactMapping.typeAndLabel(Kind.PHONE, "Boat"))
        assertEquals("type_19", ContactMapping.label(Kind.PHONE, "19", null))
        assertEquals("19" to null, ContactMapping.typeAndLabel(Kind.PHONE, "type_19"))
        assertEquals("2000-02-29", ContactMapping.startDate(ContactMapping.birthday("2000-02-29")!!))
        assertEquals("--02-29", ContactMapping.startDate(ContactMapping.birthday("--02-29")!!))
        assertNull(ContactMapping.birthday("2001-02-29"))
        assertNull(ContactMapping.birthday("Dec 10"))
    }

    private fun sources(vararg pairs: Pair<String, Long>) = JSONArray().apply {
        pairs.forEach { (core, native) -> put(JSONObject().put("field", "phones").put("id", core).put("source_id", native.toString())) }
    }

    @Test fun namePartsMergeIntoOneVersionGuardedReplaceOnTheWritableRaw() {
        val request = JSONObject().put("patches", JSONArray()
            .put(JSONObject().put("op", "replace").put("path", "name.given").put("value", "Augusta"))
            .put(JSONObject().put("op", "remove").put("path", "name.family")))
        val plan = UpdatePlanner(contact(), ContactMapping.accountId(google), JSONArray()).plan(request) as UpdatePlanner.Result.Planned
        val op = plan.ops.getValue(10).single() as FieldOp.Replace
        assertEquals(100L, op.dataId)
        assertEquals(4, op.expectedVersion)
        assertEquals(mapOf(StructuredName.GIVEN_NAME to "Augusta", StructuredName.FAMILY_NAME to null), op.values)
    }

    @Test fun itemPatchesTranslateCoreIdsAndRefuseReadOnlyOrStaleItems() {
        val replace = JSONObject().put("patches", JSONArray().put(JSONObject().put("op", "replace").put("path", "phones[c1]")
            .put("value", JSONObject().put("id", "c1").put("value", "+1999").put("label", "home"))))
        val plan = UpdatePlanner(contact(), null, sources("c1" to 101L)).plan(replace) as UpdatePlanner.Result.Planned
        val op = plan.ops.getValue(10).single() as FieldOp.Replace
        assertEquals(101L, op.dataId)
        assertEquals("+1999", op.values[Phone.NUMBER])
        assertEquals("${Phone.TYPE_HOME}", op.values[Phone.TYPE])

        val readOnly = JSONObject().put("patches", JSONArray().put(JSONObject().put("op", "remove").put("path", "phones[c2]")))
        assertEquals("read_only_account", (UpdatePlanner(contact(), null, sources("c2" to 102L)).plan(readOnly) as UpdatePlanner.Result.Rejected).reason)
        val stale = JSONObject().put("patches", JSONArray().put(JSONObject().put("op", "remove").put("path", "phones[gone]")))
        assertEquals("stale_item", (UpdatePlanner(contact(), null, sources()).plan(stale) as UpdatePlanner.Result.Rejected).reason)
    }

    @Test fun additionsGoToTheDefaultAccountRawAndBirthdayRemovalDeletesTheRow() {
        val request = JSONObject().put("patches", JSONArray()
            .put(JSONObject().put("op", "add").put("path", "emails").put("value", JSONObject().put("id", "e1").put("value", "ada@example.test")))
            .put(JSONObject().put("op", "remove").put("path", "birthday")))
        val plan = UpdatePlanner(contact(), ContactMapping.accountId(google), JSONArray()).plan(request) as UpdatePlanner.Result.Planned
        val ops = plan.ops.getValue(10)
        assertTrue(ops.any { it is FieldOp.Remove && it.dataId == 104L })
        assertTrue(ops.any { it is FieldOp.Add && it.kind == Kind.EMAIL })
        assertFalse(plan.ops.containsKey(11))
    }

    @Test fun createRequestBecomesProviderRows() {
        val fields = ContactMapping.newFields(JSONObject().put("display_name", "Grace Hopper").put("notes", "Navy")
            .put("birthday", JSONObject().put("year", 1906).put("month", 12).put("day", 9))
            .put("phones", JSONArray().put(JSONObject().put("id", "x").put("value", "+1202").put("label", "mobile"))))
        assertEquals(setOf(Kind.NAME, Kind.NOTE, Kind.EVENT, Kind.PHONE), fields.map { it.kind }.toSet())
        assertEquals("1906-12-09", fields.single { it.kind == Kind.EVENT }.values[Event.START_DATE])
        assertEquals("${Phone.TYPE_MOBILE}", fields.single { it.kind == Kind.PHONE }.values[Phone.TYPE])
    }

    @Test fun rawScopedViewHoldsOnlyTheCreatedRawAndNoLookupHint() {
        val scoped = ContactMapping.rawScoped(contact(), 11)
        assertEquals(setOf(11L), ContactMapping.rawIds(scoped))
        val entry = ContactMapping.entry(scoped, null)
        assertEquals("raw:11", entry.getString("source_key"))
        assertFalse(entry.getJSONObject("provenance").has("lookup_hint"))
        assertFalse(entry.getJSONObject("fields").has("name"))
    }

    @Test fun coreViewTranslatesItemIdsAndUnmappedItemsNeverMatch() {
        val sources = sources("c1" to 101L)
        val view = ContactMapping.coreView(contact(), sources)
        val phones = view.getJSONArray("phones")
        assertEquals("c1", phones.getJSONObject(0).getString("id"))
        assertEquals("native:102", phones.getJSONObject(1).getString("id"))
        assertTrue(ContactMapping.same(JSONObject("""{"a":1,"b":null}"""), JSONObject("""{"a":1.0}""")))
        assertFalse(ContactMapping.same(JSONObject("""{"a":[1,2]}"""), JSONObject("""{"a":[2,1]}""")))
    }

    @Test fun truncatedOrUnrepresentableValuesAreNeverEditable() {
        val longNote = "n".repeat(9000)
        val c = contact().let { it.copy(fields = it.fields + listOf(
            Field(105, 10, Kind.NOTE, mapOf(android.provider.ContactsContract.CommonDataKinds.Note.NOTE to longNote), false, false, 0),
            Field(106, 10, Kind.PHONE, mapOf(Phone.NUMBER to "9".repeat(1100), Phone.TYPE to "${Phone.TYPE_HOME}"), false, false, 0),
            Field(107, 10, Kind.EVENT, mapOf(Event.START_DATE to "1815", Event.TYPE to "${Event.TYPE_BIRTHDAY}"), false, false, 0),
        )) }
        val phones = ContactMapping.fields(c).getJSONArray("phones")
        assertFalse((0 until phones.length()).map { phones.getJSONObject(it) }.single { it.getString("id") == "106" }.getBoolean("writable"))
        fun rejected(patch: JSONObject, sources: JSONArray = JSONArray()) =
            (UpdatePlanner(c, null, sources).plan(JSONObject().put("patches", JSONArray().put(patch))) as UpdatePlanner.Result.Rejected).reason
        assertEquals("unsupported_field", rejected(JSONObject().put("op", "replace").put("path", "notes").put("value", "short")))
        assertEquals("unsupported_field", rejected(JSONObject().put("op", "remove").put("path", "phones[c6]"), sources("c6" to 106L)))
        assertEquals("unsupported_field", rejected(JSONObject().put("op", "replace").put("path", "birthday").put("value", JSONObject().put("month", 1).put("day", 2))))
    }
}
