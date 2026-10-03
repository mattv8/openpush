package dev.peppy.mobile

import android.content.ContentResolver
import android.content.Context
import android.net.Uri
import java.io.File
import java.io.FileOutputStream
import java.nio.ByteBuffer
import java.nio.charset.Charset
import java.nio.charset.CodingErrorAction

data class MmsProviderPart(val providerPartId: Long, val sequence: Int?, val mediaType: String?, val name: String?, val charset: Int?, val text: String?, val contentUri: Uri)
data class MmsProviderRecord(val id: Long, val threadId: Long?, val subscriptionId: Int?, val messageBox: Int?, val dateMs: Long?, val transactionId: String?, val subject: String?, val sender: List<String>, val toRecipients: List<String>, val ccRecipients: List<String>, val bccRecipients: List<String>, val messageType: Int?, val expiry: Long?, val parts: List<MmsProviderPart>, val ready: Boolean, val error: String?)
data class MmsProviderPage(val records: List<MmsProviderRecord>, val scannedThroughId: Long)
class MmsProviderException(message: String) : IllegalStateException(message)

/** Bounded public-provider reader. A caller checkpoints only [scannedThroughId]. */
class MmsProviderReader(context: Context) {
    private val resolver: ContentResolver = context.contentResolver
    fun page(afterId: Long, limit: Int): MmsProviderPage {
        require(afterId >= 0); val capped = limit.coerceIn(1, MAX_PAGE); val records = mutableListOf<MmsProviderRecord>(); var through = afterId
        resolver.query(MMS, arrayOf("_id"), "_id > ?", arrayOf(afterId.toString()), "_id ASC LIMIT $capped")?.use { c ->
            val column = c.getColumnIndexOrThrow("_id")
            while (c.moveToNext() && records.size < capped) { val id = c.getLong(column); through = id; records += read(id) ?: deleted(id) }
        }
        return MmsProviderPage(records, through)
    }
    fun read(id: Long): MmsProviderRecord? {
        valid(id)
        resolver.query(Uri.withAppendedPath(MMS, "$id"), MESSAGE, null, null, null)?.use { c ->
            if (!c.moveToFirst()) return null
            val type = c.long("m_type")?.toInt(); val addresses = addresses(id)
            var error: String? = when { type == 130 -> "provider_content_pending"; type !in setOf(128, 132) -> "unsupported_type"; c.long("date") == null -> "provider_content_pending"; else -> null }
            val metadata = listOf(c.string("tr_id"), c.string("sub")); if (metadata.any { it != null && it.toByteArray().size > MAX_STRING }) error = "metadata_overflow"
            if (addresses.error != null) error = addresses.error
            val parts = if (error == null) parts(id) else PartResult(emptyList(), null)
            if (parts.error != null) error = parts.error
            if (error == null && parts.value.isEmpty()) error = "provider_content_pending"
            val sub = c.long("sub_id")?.toInt()?.takeIf { it >= 0 }
            return MmsProviderRecord(id, c.long("thread_id"), sub, c.long("msg_box")?.toInt(), c.long("date")?.times(1000), bounded(c.string("tr_id")), bounded(c.string("sub")), addresses.from, addresses.to, addresses.cc, addresses.bcc, type, c.long("exp"), parts.value, error == null, error)
        }; return null
    }
    fun readPartText(part: MmsProviderPart, byteLimit: Int = MAX_TEXT): String? {
        require(byteLimit in 1..MAX_TEXT); valid(part.providerPartId); val charset = charset(part.charset)
        val bytes = part.text?.toByteArray(charset) ?: resolver.openInputStream(partUri(part.providerPartId))?.use { input -> boundedBytes(input, byteLimit) } ?: return null
        if (bytes.size > byteLimit) throw MmsProviderException("part text exceeds budget")
        return charset.newDecoder().onMalformedInput(CodingErrorAction.REPORT).onUnmappableCharacter(CodingErrorAction.REPORT).decode(ByteBuffer.wrap(bytes)).toString()
    }
    fun copyPart(part: MmsProviderPart, destination: File, byteLimit: Long): Long {
        require(byteLimit in 1..MAX_PART); valid(part.providerPartId); var total = 0L; val temporary = File(destination.parentFile, ".${destination.name}.part")
        try { resolver.openInputStream(partUri(part.providerPartId))?.use { input -> FileOutputStream(temporary).use { output ->
            val buffer = ByteArray(16384); while (true) { val n = input.read(buffer); if (n < 0) break; total += n; if (total > byteLimit) throw MmsProviderException("part exceeds budget"); output.write(buffer, 0, n) }; output.fd.sync()
        } } ?: throw MmsProviderException("part unavailable")
            if (!temporary.renameTo(destination)) throw MmsProviderException("part copy failed"); return total
        } catch (e: Exception) { temporary.delete(); destination.delete(); throw e }
    }
    private fun parts(message: Long): PartResult {
        val values = mutableListOf<MmsProviderPart>(); var error: String? = null
        resolver.query(PART, PART_COLUMNS, "mid=?", arrayOf("$message"), "seq ASC, _id ASC")?.use { c -> while (c.moveToNext()) {
            if (values.size >= MAX_PARTS) { error = "too_many_parts"; continue }; val id = c.long("_id") ?: continue; valid(id)
            val name = c.string("name") ?: c.string("fn"); if (name != null && name.toByteArray().size > MAX_STRING) { error = "metadata_overflow"; continue }
            values += MmsProviderPart(id, c.long("seq")?.toInt(), bounded(c.string("ct")), name, c.long("chset")?.toInt(), bounded(c.string("text")), partUri(id))
        } }; return PartResult(values, error)
    }
    private fun addresses(message: Long): AddressResult {
        val from = mutableListOf<String>(); val to = mutableListOf<String>(); val cc = mutableListOf<String>(); val bcc = mutableListOf<String>(); var error: String? = null
        resolver.query(Uri.withAppendedPath(Uri.withAppendedPath(MMS, "$message"), "addr"), arrayOf("address", "type"), "type IN (137,151,130,129)", null, null)?.use { c -> val a = c.getColumnIndexOrThrow("address"); val t = c.getColumnIndexOrThrow("type"); while (c.moveToNext()) {
            val value = c.getString(a) ?: continue; if (value.toByteArray().size > MAX_STRING || from.size + to.size + cc.size + bcc.size >= MAX_ADDRESSES) { error = "address_overflow"; continue }; when (c.getInt(t)) { 137 -> from += value; 151 -> to += value; 130 -> cc += value; 129 -> bcc += value }
        } }; return AddressResult(from, to, cc, bcc, error)
    }
    private fun bounded(value: String?) = value?.takeIf { it.toByteArray().size <= MAX_STRING }
    private fun boundedBytes(input: java.io.InputStream, limit: Int): ByteArray { val out = java.io.ByteArrayOutputStream(); val b = ByteArray(4096); while (true) { val n=input.read(b); if(n<0)break; if(out.size()+n>limit) throw MmsProviderException("part text exceeds budget"); out.write(b,0,n) }; return out.toByteArray() }
    private fun charset(mib: Int?): Charset = when (mib) { null, 106 -> Charsets.UTF_8; 3 -> Charsets.US_ASCII; 4 -> Charsets.ISO_8859_1; 17 -> Charset.forName("Shift_JIS"); 38 -> Charset.forName("EUC-KR"); 1013 -> Charset.forName("UTF-16BE"); 1014 -> Charset.forName("UTF-16LE"); 1015 -> Charsets.UTF_16; 2025 -> Charset.forName("GB2312"); 2026 -> Charset.forName("Big5"); else -> throw MmsProviderException("unsupported_charset") }
    private fun deleted(id: Long) = MmsProviderRecord(id,null,null,null,null,null,null,emptyList(),emptyList(),emptyList(),emptyList(),null,null,emptyList(),false,"provider_row_deleted")
    private fun valid(id: Long) { require(id > 0) }; private fun partUri(id: Long)=Uri.withAppendedPath(PART,"$id")
    private fun android.database.Cursor.string(n:String)=getColumnIndex(n).takeIf{it>=0}?.let{getString(it)}; private fun android.database.Cursor.long(n:String)=getColumnIndex(n).takeIf{it>=0&&!isNull(it)}?.let{getLong(it)}
    private data class AddressResult(val from:List<String>,val to:List<String>,val cc:List<String>,val bcc:List<String>,val error:String?); private data class PartResult(val value:List<MmsProviderPart>,val error:String?)
    companion object { private val MMS=Uri.parse("content://mms"); private val PART=Uri.parse("content://mms/part"); private val MESSAGE=arrayOf("_id","thread_id","sub_id","msg_box","date","tr_id","sub","m_type","exp"); private val PART_COLUMNS=arrayOf("_id","seq","ct","name","fn","chset","text"); const val MAX_PAGE=50; const val MAX_PARTS=10; const val MAX_TEXT=256*1024; const val MAX_PART=32L*1024*1024; const val MAX_STRING=4096; const val MAX_ADDRESSES=100 }
}
