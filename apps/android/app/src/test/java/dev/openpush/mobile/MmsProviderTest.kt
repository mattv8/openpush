package dev.openpush.mobile

import android.content.ContentProvider
import android.content.ContentValues
import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowContentResolver

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class MmsProviderTest {
    private lateinit var provider: FakeMmsProvider
    private lateinit var reader: MmsProviderReader

    @Before fun setUp() {
        provider = Robolectric.buildContentProvider(FakeMmsProvider::class.java).create().get()
        ShadowContentResolver.registerProviderInternal("mms", provider)
        reader = MmsProviderReader(ApplicationProvider.getApplicationContext())
    }

    @Test fun preservesAddressGroupsAndInvalidSubscriptionIsNull() {
        provider.messages += message(1, 128, sub = -1)
        provider.addresses[1] = listOf("from" to 137, "to-1" to 151, "to-2" to 151, "cc" to 130, "bcc" to 129)
        provider.parts[1] = listOf(part(10))
        val record = reader.read(1)!!
        assertEquals(listOf("from"), record.sender); assertEquals(listOf("to-1", "to-2"), record.toRecipients)
        assertEquals(listOf("cc"), record.ccRecipients); assertEquals(listOf("bcc"), record.bccRecipients)
        assertNull(record.subscriptionId)
    }

    @Test fun keepsPendingAndUnsupportedRowsVisibleWithoutPassingOutboxCheckpoint() {
        provider.messages += message(5, 128, box = 4)
        provider.messages += message(6, 134)
        provider.parts[5] = listOf(part(50)); provider.parts[6] = listOf(part(60))
        val page = reader.page(0, 50)
        assertEquals(6L, page.scannedThroughId)
        assertEquals("unsupported_type", page.records.single { it.id == 6L }.error)
        assertTrue(page.records.any { it.id == 5L })
    }

    @Test fun pendingUnsupportedZeroAndTooManyPartsAreRecordsAndPageContinues() {
        provider.messages += message(1, 130)
        provider.messages += message(2, 134)
        provider.messages += message(3, 128)
        provider.messages += message(4, 128)
        provider.parts[4] = (1..11).map { part(it.toLong()) }
        provider.messages += message(5, 128); provider.parts[5] = listOf(part(50))
        val page = reader.page(0, 50)
        assertEquals("provider_content_pending", page.records.single { it.id == 1L }.error)
        assertEquals("unsupported_type", page.records.single { it.id == 2L }.error)
        assertEquals("provider_content_pending", page.records.single { it.id == 3L }.error)
        assertEquals("too_many_parts", page.records.single { it.id == 4L }.error)
        assertTrue(page.records.single { it.id == 5L }.ready)
    }

    @Test fun capsRowsWhenProviderIgnoresLimitAndFlagsOversizedFields() {
        repeat(60) { n -> provider.messages += message(n.toLong() + 1, 128) ; provider.parts[n.toLong() + 1] = listOf(part(n.toLong() + 1)) }
        assertEquals(50, reader.page(0, 50).records.size)
        provider.messages.clear(); provider.parts.clear(); provider.addresses.clear()
        provider.messages += message(1, 128, subject = "x".repeat(MmsProviderReader.MAX_STRING + 1)); provider.parts[1] = listOf(part(1))
        assertEquals("metadata_overflow", reader.read(1)!!.error)
        provider.messages.clear(); provider.messages += message(2, 128); provider.parts[2] = listOf(part(2)); provider.addresses[2] = listOf("x".repeat(MmsProviderReader.MAX_STRING + 1) to 151)
        assertEquals("address_overflow", reader.read(2)!!.error)
    }

    @Test fun decodesKnownCharsetsAndRejectsOversizedInlineText() {
        val utf16 = MmsProviderPart(1, null, "text/plain", null, 1015, "hello", Uri.parse("content://mms/part/1"))
        assertEquals("hello", reader.readPartText(utf16))
        val oversized = utf16.copy(text = "x".repeat(MmsProviderReader.MAX_TEXT + 1))
        assertThrows(MmsProviderException::class.java) { reader.readPartText(oversized) }
        assertThrows(MmsProviderException::class.java) { reader.readPartText(utf16.copy(charset = 9999)) }
    }

    private fun message(id: Long, type: Int, box: Int = 1, sub: Int = 1, subject: String? = null) = Msg(id, type, box, sub, subject)
    private fun part(id: Long) = Part(id)

    data class Msg(val id: Long, val type: Int, val box: Int, val sub: Int, val subject: String?)
    data class Part(val id: Long)
    class FakeMmsProvider : ContentProvider() {
        val messages = mutableListOf<Msg>(); val parts = mutableMapOf<Long, List<Part>>(); val addresses = mutableMapOf<Long, List<Pair<String, Int>>>()
        override fun onCreate() = true
        override fun query(uri: Uri, projection: Array<out String>?, selection: String?, selectionArgs: Array<out String>?, sortOrder: String?): Cursor {
            val segments = uri.pathSegments
            return when {
                segments.isEmpty() -> MatrixCursor(arrayOf("_id")).also { c -> messages.sortedBy { it.id }.forEach { c.addRow(arrayOf(it.id)) } }
                segments.size == 2 && segments[1] == "addr" -> MatrixCursor(arrayOf("address", "type")).also { c -> addresses[segments[0].toLong()].orEmpty().forEach { c.addRow(arrayOf(it.first, it.second)) } }
                segments.firstOrNull() == "part" -> MatrixCursor(arrayOf("_id", "seq", "ct", "name", "fn", "chset", "text")).also { c ->
                    parts[selectionArgs!!.single().toLong()].orEmpty().forEachIndexed { i, p -> c.addRow(arrayOf(p.id, i, "text/plain", null, null, 106, null)) }
                }
                else -> MatrixCursor(arrayOf("_id", "thread_id", "sub_id", "msg_box", "date", "tr_id", "sub", "m_type", "exp")).also { c ->
                    messages.find { it.id == segments.single().toLong() }?.let { m -> c.addRow(arrayOf(m.id, 1, m.sub, m.box, 1, null, m.subject, m.type, null)) }
                }
            }
        }
        override fun getType(uri: Uri): String? = null
        override fun insert(uri: Uri, values: ContentValues?): Uri? = null
        override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?): Int = 0
        override fun update(uri: Uri, values: ContentValues?, selection: String?, selectionArgs: Array<out String>?): Int = 0
    }
}
