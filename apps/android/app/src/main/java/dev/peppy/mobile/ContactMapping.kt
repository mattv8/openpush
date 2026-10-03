package dev.peppy.mobile

import android.provider.ContactsContract.CommonDataKinds.Email
import android.provider.ContactsContract.CommonDataKinds.Event
import android.provider.ContactsContract.CommonDataKinds.Nickname
import android.provider.ContactsContract.CommonDataKinds.Note
import android.provider.ContactsContract.CommonDataKinds.Organization
import android.provider.ContactsContract.CommonDataKinds.Phone
import android.provider.ContactsContract.CommonDataKinds.StructuredName
import android.provider.ContactsContract.CommonDataKinds.StructuredPostal
import dev.peppy.mobile.AndroidContactsProvider.Contact
import dev.peppy.mobile.AndroidContactsProvider.Field
import dev.peppy.mobile.AndroidContactsProvider.Kind
import dev.peppy.mobile.AndroidContactsProvider.NewField
import dev.peppy.mobile.AndroidContactsProvider.RawSource
import org.json.JSONArray
import org.json.JSONObject
import java.security.MessageDigest

/**
 * Pure conversion between provider rows and the core's common contact fields. Item IDs sent to
 * core are provider `Data._ID`s (core maps them to its own stable field IDs); labels are canonical
 * tokens for Android's standard types and verbatim text for custom labels.
 */
internal object ContactMapping {
    const val LOCAL_ACCOUNT_ID = "android:local"
    private const val MAX_TEXT = 1024
    private const val MAX_NOTE_BYTES = 8192
    private const val MAX_VALUES = 20
    const val MAX_LIST_VALUES = MAX_VALUES

    fun accountId(account: ContactAccount?): String = if (account == null) LOCAL_ACCOUNT_ID else {
        val digest = MessageDigest.getInstance("SHA-256").digest("${account.type}\u0000${account.name}".toByteArray(Charsets.UTF_8))
        "android:" + digest.take(16).joinToString("") { "%02x".format(it) }
    }

    fun accountName(account: ContactAccount?): String = when {
        account == null -> "Phone"
        account.type == "com.google" -> "Google · ${account.name}"
        else -> "${account.type} · ${account.name}"
    }

    private val PHONE_TYPES = mapOf(
        Phone.TYPE_HOME to "home", Phone.TYPE_MOBILE to "mobile", Phone.TYPE_WORK to "work", Phone.TYPE_FAX_WORK to "work_fax",
        Phone.TYPE_FAX_HOME to "home_fax", Phone.TYPE_PAGER to "pager", Phone.TYPE_OTHER to "other", Phone.TYPE_MAIN to "main",
        Phone.TYPE_OTHER_FAX to "other_fax", Phone.TYPE_WORK_MOBILE to "work_mobile", Phone.TYPE_WORK_PAGER to "work_pager",
    )
    private val EMAIL_TYPES = mapOf(Email.TYPE_HOME to "home", Email.TYPE_WORK to "work", Email.TYPE_OTHER to "other", Email.TYPE_MOBILE to "mobile")
    private val POSTAL_TYPES = mapOf(StructuredPostal.TYPE_HOME to "home", StructuredPostal.TYPE_WORK to "work", StructuredPostal.TYPE_OTHER to "other")

    private fun types(kind: Kind) = when (kind) {
        Kind.PHONE -> PHONE_TYPES
        Kind.EMAIL -> EMAIL_TYPES
        else -> POSTAL_TYPES
    }

    /** Provider (TYPE, LABEL) -> core label token. */
    fun label(kind: Kind, type: String?, custom: String?): String? {
        val code = type?.toIntOrNull() ?: return custom?.take(MAX_TEXT)
        if (code == 0) return custom?.takeIf { it.isNotBlank() }?.take(MAX_TEXT)
        return types(kind)[code] ?: "type_$code"
    }

    /** Core label token -> provider (TYPE, LABEL). Unknown text becomes a custom label. */
    fun typeAndLabel(kind: Kind, token: String?): Pair<String, String?> {
        val default = when (kind) {
            Kind.PHONE -> Phone.TYPE_MOBILE
            Kind.EMAIL -> Email.TYPE_OTHER
            else -> StructuredPostal.TYPE_HOME
        }
        if (token.isNullOrBlank()) return default.toString() to null
        types(kind).entries.firstOrNull { it.value == token }?.let { return it.key.toString() to null }
        token.removePrefix("type_").takeIf { token.startsWith("type_") }?.toIntOrNull()?.let { return it.toString() to null }
        return "0" to token.take(MAX_TEXT)
    }

    private val PARTIAL = Regex("""^--(\d{2})-(\d{2})$""")
    private val FULL = Regex("""^(\d{4})-(\d{2})-(\d{2})""")

    fun birthday(startDate: String?): JSONObject? {
        if (startDate == null) return null
        PARTIAL.find(startDate)?.let { m -> return validDate(null, m.groupValues[1].toInt(), m.groupValues[2].toInt()) }
        FULL.find(startDate)?.let { m -> return validDate(m.groupValues[1].toInt(), m.groupValues[2].toInt(), m.groupValues[3].toInt()) }
        return null
    }

    private fun validDate(year: Int?, month: Int, day: Int): JSONObject? {
        if (year != null && year !in 1..9999) return null
        val leap = year == null || (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
        val days = when (month) { 1, 3, 5, 7, 8, 10, 12 -> 31; 4, 6, 9, 11 -> 30; 2 -> if (leap) 29 else 28; else -> return null }
        if (day !in 1..days) return null
        return JSONObject().apply { if (year != null) put("year", year); put("month", month); put("day", day) }
    }

    fun startDate(birthday: JSONObject): String? {
        val month = birthday.optInt("month", 0)
        val day = birthday.optInt("day", 0)
        val year = if (birthday.has("year") && !birthday.isNull("year")) birthday.getInt("year") else null
        validDate(year, month, day) ?: return null
        return if (year == null) "--%02d-%02d".format(month, day) else "%04d-%02d-%02d".format(year, month, day)
    }

    /** The writable raw contact that receives additions: default account, then local, then first. */
    fun targetRaw(contact: Contact, defaultAccountId: String?): RawSource? {
        val writable = contact.sources.filter { it.writable }
        return writable.firstOrNull { accountId(it.account) == defaultAccountId }
            ?: writable.firstOrNull { it.account == null }
            ?: writable.firstOrNull()
    }

    /** Rows of [kind], writable raw contacts first (stable provider order otherwise). */
    fun ordered(contact: Contact, kind: Kind): List<Field> =
        contact.fields.filter { it.kind == kind }.sortedBy { if (it.readOnly) 1 else 0 }

    private fun text(value: String?) = value?.takeIf { it.isNotBlank() }?.take(MAX_TEXT)

    /** How a capture treats the stored contact photo (core platform-capture semantics). */
    sealed interface PhotoField {
        /** `photo` omitted: core keeps the stored photo (text-only capture is a no-op for photos). */
        data object Inherit : PhotoField
        /** `photo: null`: the provider has no (usable) photo. */
        data object Clear : PhotoField
        /** `photo: {attachment_id}` of a prepared, tracked contact photo. */
        data class Set(val attachmentId: String) : PhotoField
    }

    /** Core `fields` for a provider contact. */
    fun fields(contact: Contact, photo: PhotoField = PhotoField.Inherit): JSONObject {
        val out = JSONObject().put("display_name", contact.displayName.take(MAX_TEXT))
        ordered(contact, Kind.NAME).firstOrNull()?.let { row ->
            val name = JSONObject()
            for ((key, column) in NAME_PARTS) text(row.values[column])?.let { name.put(key, it) }
            if (name.length() > 0) out.put("name", name)
        }
        ordered(contact, Kind.NICKNAME).firstNotNullOfOrNull { text(it.values[Nickname.NAME]) }?.let { out.put("nickname", it) }
        ordered(contact, Kind.ORGANIZATION).firstOrNull()?.let { row ->
            text(row.values[Organization.COMPANY])?.let { out.put("organization", it) }
            text(row.values[Organization.TITLE])?.let { out.put("title", it) }
        }
        ordered(contact, Kind.NOTE).firstNotNullOfOrNull { it.values[Note.NOTE]?.takeIf(String::isNotBlank) }?.let { out.put("notes", truncateBytes(it, MAX_NOTE_BYTES)) }
        ordered(contact, Kind.EVENT)
            .filter { it.values[Event.TYPE] == Event.TYPE_BIRTHDAY.toString() }
            .firstNotNullOfOrNull { birthday(it.values[Event.START_DATE]) }?.let { out.put("birthday", it) }
        list(contact, Kind.PHONE, Phone.NUMBER)?.let { out.put("phones", it) }
        list(contact, Kind.EMAIL, Email.ADDRESS)?.let { out.put("emails", it) }
        list(contact, Kind.POSTAL, StructuredPostal.FORMATTED_ADDRESS)?.let { out.put("addresses", it) }
        when (photo) {
            PhotoField.Inherit -> Unit
            PhotoField.Clear -> out.put("photo", JSONObject.NULL)
            is PhotoField.Set -> out.put("photo", JSONObject().put("attachment_id", photo.attachmentId))
        }
        return out
    }

    private fun list(contact: Contact, kind: Kind, valueColumn: String): JSONArray? {
        val seen = HashSet<String>()
        val items = JSONArray()
        for (row in ordered(contact, kind)) {
            if (items.length() == MAX_VALUES) break
            val value = text(row.values[valueColumn]) ?: if (kind == Kind.POSTAL) "" else continue
            if (!seen.add(value.trim().lowercase()) && value.isNotEmpty()) continue
            // Values the core cannot hold whole are shown but never editable (fail closed).
            val item = JSONObject().put("id", row.dataId.toString()).put("value", value).put("writable", !row.readOnly && representable(row))
            label(kind, row.values[DATA_TYPE], row.values[DATA_LABEL])?.let { item.put("label", it) }
            if (kind == Kind.POSTAL) for ((key, column) in POSTAL_PARTS) text(row.values[column])?.let { item.put(key, it) }
            items.put(item)
        }
        return items.takeIf { it.length() > 0 }
    }

    /**
     * Stable capture identity: the lowest raw contact id of the captured aggregate. Raw ids
     * survive renames (unlike LOOKUP_KEY, which embeds the display name for raws without a
     * SOURCE_ID); joins/splits change the raw set and are detected before any write.
     */
    fun sourceKey(contact: Contact): String = "raw:" + contact.sources.minOf { it.rawContactId }

    fun rawIds(contact: Contact): Set<Long> = contact.sources.mapTo(HashSet()) { it.rawContactId }

    /**
     * One `capture_platform_contacts_json` / `observe` platform entry. Owner-local provenance
     * records the exact captured raw set (`raw_ids`) and the LOOKUP_KEY only as a hint.
     * [photoSourceHash] is the opaque provider photo fingerprint recorded with a given photo.
     */
    fun entry(
        contact: Contact,
        defaultAccountId: String?,
        photo: PhotoField = PhotoField.Inherit,
        photoSourceHash: String? = null,
    ): JSONObject {
        val target = targetRaw(contact, defaultAccountId)
        val provenance = JSONObject()
            .put("account_id", accountId((target ?: contact.sources.firstOrNull())?.account))
            .put("read_only", target == null)
            .put("raw_ids", JSONArray(rawIds(contact).sorted().map(Long::toString)))
        if (contact.lookupKey.isNotEmpty()) provenance.put("lookup_hint", contact.lookupKey.take(1024))
        if (photoSourceHash != null) provenance.put("photo_source_hash", photoSourceHash)
        return JSONObject().put("source_key", sourceKey(contact)).put("fields", fields(contact, photo)).put("provenance", provenance)
    }

    /**
     * Only [rawContactId]'s part of an aggregate (used when a created raw was auto-joined with
     * pre-existing raws: the new core contact must describe what was created, never them).
     * No lookup hint, so it can never claim another mapping.
     */
    fun rawScoped(contact: Contact, rawContactId: Long): Contact {
        val fields = contact.fields.filter { it.rawContactId == rawContactId }
        val name = fields.firstOrNull { it.kind == Kind.NAME }?.values?.get(StructuredName.DISPLAY_NAME).orEmpty()
        return contact.copy(
            lookupKey = "",
            displayName = name,
            photoDataId = contact.photoDataId?.takeIf { id -> fields.any { it.dataId == id } },
            sources = contact.sources.filter { it.rawContactId == rawContactId },
            fields = fields,
        )
    }

    /** Whether the core can hold this provider row's values without truncation. */
    fun representable(row: Field): Boolean = when (row.kind) {
        Kind.NOTE -> (row.values[Note.NOTE]?.toByteArray(Charsets.UTF_8)?.size ?: 0) <= MAX_NOTE_BYTES
        Kind.EVENT -> row.values[Event.TYPE] != Event.TYPE_BIRTHDAY.toString() || birthday(row.values[Event.START_DATE]) != null
        Kind.PHOTO -> true
        else -> row.values.values.all { (it?.length ?: 0) <= MAX_TEXT }
    }

    /** Roots of the core contact body that a fresh provider state is compared on. */
    val ROOTS = listOf("display_name", "name", "nickname", "organization", "title", "notes", "birthday", "phones", "emails", "addresses")

    /**
     * The provider contact in core terms: [fields] with provider item ids translated to core
     * field ids through the owner's `field_sources`. An item without a mapping keeps a
     * `native:` id, so it never equals a stored item.
     */
    fun coreView(contact: Contact, fieldSources: JSONArray): JSONObject {
        val toCore = (0 until fieldSources.length()).associate { index ->
            val source = fieldSources.getJSONObject(index)
            (source.optString("field") to source.optString("source_id")) to source.optString("id")
        }
        val view = fields(contact)
        for (list in listOf("phones", "emails", "addresses")) {
            val items = view.optJSONArray(list) ?: continue
            for (index in 0 until items.length()) {
                val item = items.getJSONObject(index)
                val native = item.getString("id")
                item.put("id", toCore[list to native] ?: "native:$native")
            }
        }
        return view
    }

    /** Order-insensitive JSON equality for objects (absent == null), ordered for arrays. */
    fun same(a: Any?, b: Any?): Boolean {
        val x = if (a == JSONObject.NULL) null else a
        val y = if (b == JSONObject.NULL) null else b
        return when {
            x == null || y == null -> x == null && y == null
            x is JSONObject && y is JSONObject -> (x.keys().asSequence() + y.keys().asSequence()).toSet().all { same(x.opt(it), y.opt(it)) }
            x is JSONArray && y is JSONArray -> x.length() == y.length() && (0 until x.length()).all { same(x.opt(it), y.opt(it)) }
            x is Number && y is Number -> x.toString().toBigDecimal().compareTo(y.toString().toBigDecimal()) == 0
            else -> x == y
        }
    }

    /** Provider values for a core list item (`phones`/`emails`/`addresses`). */
    fun listValues(kind: Kind, item: JSONObject): Map<String, String?> {
        val (type, custom) = typeAndLabel(kind, item.optString("label").takeIf { item.has("label") && !item.isNull("label") })
        val values = mutableMapOf<String, String?>(DATA_TYPE to type, DATA_LABEL to custom)
        val value = item.optString("value").takeIf { item.has("value") && !item.isNull("value") }
        when (kind) {
            Kind.PHONE -> values[Phone.NUMBER] = value
            Kind.EMAIL -> values[Email.ADDRESS] = value
            else -> {
                values[StructuredPostal.FORMATTED_ADDRESS] = value?.takeIf { it.isNotBlank() }
                for ((key, column) in POSTAL_PARTS) values[column] = item.optString(key).takeIf { item.has(key) && !item.isNull(key) }
            }
        }
        return values
    }

    /** Provider rows for a core create request's top-level contact fields. */
    fun newFields(request: JSONObject): List<NewField> {
        val out = mutableListOf<NewField>()
        val name = request.optJSONObject("name")
        val parts = NAME_PARTS.mapNotNull { (key, column) -> name?.optString(key)?.takeIf { it.isNotEmpty() }?.let { column to it } }.toMap()
        val display = request.optString("display_name").takeIf { it.isNotBlank() }
        if (parts.isNotEmpty()) out += NewField(Kind.NAME, parts)
        else if (display != null) out += NewField(Kind.NAME, mapOf(StructuredName.DISPLAY_NAME to display))
        request.optString("nickname").takeIf { it.isNotBlank() }?.let { out += NewField(Kind.NICKNAME, mapOf(Nickname.NAME to it)) }
        val company = request.optString("organization").takeIf { it.isNotBlank() }
        val title = request.optString("title").takeIf { it.isNotBlank() }
        if (company != null || title != null) out += NewField(Kind.ORGANIZATION, mapOf(Organization.COMPANY to company, Organization.TITLE to title).filterValues { it != null })
        request.optString("notes").takeIf { it.isNotBlank() }?.let { out += NewField(Kind.NOTE, mapOf(Note.NOTE to it)) }
        request.optJSONObject("birthday")?.let(::startDate)?.let {
            out += NewField(Kind.EVENT, mapOf(Event.START_DATE to it, Event.TYPE to Event.TYPE_BIRTHDAY.toString()))
        }
        for ((key, kind) in listOf("phones" to Kind.PHONE, "emails" to Kind.EMAIL, "addresses" to Kind.POSTAL)) {
            val items = request.optJSONArray(key) ?: continue
            for (index in 0 until items.length()) out += NewField(kind, listValues(kind, items.getJSONObject(index)).filterValues { it != null })
        }
        return out
    }

    /** Longest UTF-8 prefix of at most [maxBytes] bytes (never splits a code point). */
    private fun truncateBytes(value: String, maxBytes: Int): String {
        val bytes = value.toByteArray(Charsets.UTF_8)
        if (bytes.size <= maxBytes) return value
        var cut = maxBytes
        while (cut > 0 && (bytes[cut].toInt() and 0xC0) == 0x80) cut--
        return String(bytes, 0, cut, Charsets.UTF_8)
    }

    const val DATA_TYPE = "data2"
    const val DATA_LABEL = "data3"
    val NAME_PARTS = listOf(
        "given" to StructuredName.GIVEN_NAME, "family" to StructuredName.FAMILY_NAME, "middle" to StructuredName.MIDDLE_NAME,
        "prefix" to StructuredName.PREFIX, "suffix" to StructuredName.SUFFIX,
    )
    val POSTAL_PARTS = listOf(
        "street" to StructuredPostal.STREET, "po_box" to StructuredPostal.POBOX, "neighborhood" to StructuredPostal.NEIGHBORHOOD,
        "city" to StructuredPostal.CITY, "state" to StructuredPostal.REGION, "postal_code" to StructuredPostal.POSTCODE,
        "country" to StructuredPostal.COUNTRY,
    )
}
