package dev.peppy.mobile.mms.pdu

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.nio.file.Files

/** Goldens are hand-derived WSP/OMA-MMS-ENC vectors, not a decoder of this codec. */
class MmsPduComposerTest {
    @Test fun `text only golden has AOSP From PLMN and UTF8 charset`() = inTemp { dir ->
        val out = File(dir, "message.pdu")
        MmsPduComposer.compose(request(out, text = "hi"))
        // Message-type, transaction-id, version, From(value-length=1, insert=81), To(PLMN), class, mixed, 1 part.
        val expected = hex("8c809874782d313233008d92890181972b31353535313233343536372f545950453d504c4d4e008a8084a301" +
            // Part header length=13: text/plain NUL, charset parameter 81 ea; data length=2, bytes 'hi'.
            "0e020d746578742f706c61696e0081ea6869")
        assertGolden(expected, out.readBytes())
    }

    @Test fun `group To Cc and image golden retain binary name location and CID`() = inTemp { dir ->
        val image = File(dir, "source").apply { writeBytes(byteArrayOf(1, 2, 3)) }
        val out = File(dir, "message.pdu")
        MmsPduComposer.compose(request(out, recipients = listOf(
            MmsRecipient("+15551234567", MmsRecipientKind.TO), MmsRecipient("me@example.com", MmsRecipientKind.CC),
        ), text = null, parts = listOf(MmsFilePart(image, "image/jpeg", "My photo.jpg", "img-1"))))
        // Content-Type general-form length 25, P_DEP_NAME 85; then P_CONTENT_LOCATION 8e and P_CONTENT_ID c0 quoted.
        val expected = hex("8c809874782d313233008d92890181972b31353535313233343536372f545950453d504c4d4e00826d65406578616d706c652e636f6d008a8084a301" +
            "320319696d6167652f6a70656700854d792070686f746f2e6a7067008e4d792070686f746f2e6a706700c0223c696d672d313e00010203")
        assertGolden(expected, out.readBytes())
    }

    @Test fun `subject value length quote charset and unicode first character goldens`() = inTemp { dir ->
        fun bytes(subject: String): ByteArray { val output = File(dir, "${subject.length}.pdu"); MmsPduComposer.compose(request(output, subject = subject)); return output.readBytes() }
        // 28 bytes: encoded-string Value-length 30 (1e), UTF-8 short-integer ea.
        assertTrue(bytes("a".repeat(28)).contains(hex("961eea" + "61".repeat(28) + "00")))
        // 29 bytes: Value-length quote 1f followed by uintvar 1f.
        assertTrue(bytes("a".repeat(29)).contains(hex("961f1fea" + "61".repeat(29) + "00")))
        // 200 bytes: quote + multi-octet uintvar 81 4a for length 202.
        assertTrue(bytes("a".repeat(200)).contains(hex("961f814aea" + "61".repeat(200) + "00")))
        // Non-ASCII first byte uses Text-string quote 7f before UTF-8 c3 89.
        assertTrue(bytes("É").contains(hex("9605ea7fc38900")))
    }

    @Test fun `bounds duplicate addresses empty text zero file and exact output limit`() = inTemp { dir ->
        val zero = File(dir, "zero").apply { writeBytes(byteArrayOf()) }
        val base = File(dir, "base.pdu")
        val request = request(base, text = "", parts = listOf(MmsFilePart(zero, "image/jpeg", "photo name.jpg", "zero")))
        val size = MmsPduComposer.compose(request).byteSize
        val exact = File(dir, "exact.pdu")
        assertEquals(size, MmsPduComposer.compose(request.copy(outputFile = exact, maximumPduBytes = size)).byteSize)
        assertFails { MmsPduComposer.compose(request.copy(outputFile = File(dir, "small.pdu"), maximumPduBytes = size - 1)) }
        assertTrue(MmsPduComposer.compose(request(base, recipients = List(20) { MmsRecipient("+1555123$it", MmsRecipientKind.TO) })).byteSize > 0)
        assertFails { MmsPduComposer.compose(request(base, recipients = List(21) { MmsRecipient("+1555123$it", MmsRecipientKind.TO) })) }
        assertFails { MmsPduComposer.compose(request(base, recipients = listOf(MmsRecipient("+15551234567", MmsRecipientKind.TO), MmsRecipient("+15551234567", MmsRecipientKind.CC)))) }
        assertFails { MmsPduComposer.compose(request(base, parts = List(11) { MmsFilePart(zero, "image/jpeg", null, null) })) }
        assertFails { MmsPduComposer.compose(request(base, text = null)) }
    }

    @Test fun `rejects malformed input and preserves old destination with cleanup`() = inTemp { dir ->
        val output = File(dir, "message.pdu").apply { writeText("old") }
        val bad = listOf(
            request(output, recipients = listOf(MmsRecipient("+1/TYPE=PLMN", MmsRecipientKind.TO))),
            request(output, recipients = listOf(MmsRecipient("+1\r\nX", MmsRecipientKind.TO))),
            request(output, subject = "bad\u0000header"),
            request(output, parts = listOf(MmsFilePart(File(dir, "missing"), "image/jpeg", null, null))),
            request(output, parts = listOf(MmsFilePart(File(dir, "missing"), "application/smil", null, null))),
        )
        bad.forEach { assertFails { MmsPduComposer.compose(it) }; assertEquals("old", output.readText()) }
        assertFalse(dir.listFiles()!!.any { it.name.startsWith(".mms-pdu-") })
    }

    @Test fun `rejects source growth after channel size and cleans temporary output`() = inTemp { dir ->
        val source = File(dir, "source").apply { writeBytes(byteArrayOf(1, 2, 3)) }
        val output = File(dir, "message.pdu").apply { writeText("old") }
        MmsPduComposer.afterPartSizeReadForTest = { it.appendBytes(byteArrayOf(4)) }
        try {
            assertFails { MmsPduComposer.compose(request(output, text = null, parts = listOf(MmsFilePart(source, "image/jpeg", null, null)))) }
            assertEquals("old", output.readText())
            assertFalse(dir.listFiles()!!.any { it.name.startsWith(".mms-pdu-") })
        } finally {
            MmsPduComposer.afterPartSizeReadForTest = null
        }
    }

    private fun request(output: File, recipients: List<MmsRecipient> = listOf(MmsRecipient("+15551234567", MmsRecipientKind.TO)), subject: String? = null, text: String? = "hi", parts: List<MmsFilePart> = emptyList(), maximumPduBytes: Long = 300_000) =
        MmsPduRequest("tx-123", recipients, subject, text, parts, maximumPduBytes, output)
    private fun assertFails(action: () -> Unit) { try { action(); throw AssertionError("expected failure") } catch (_: MmsPduException) {} }
    private fun inTemp(block: (File) -> Unit) { val dir = Files.createTempDirectory("mms-pdu-test").toFile(); try { block(dir) } finally { dir.deleteRecursively() } }
}

private fun hex(value: String): ByteArray = value.chunked(2).map { it.toInt(16).toByte() }.toByteArray()
private fun ByteArray.contains(needle: ByteArray): Boolean = indices.any { start -> start + needle.size <= size && copyOfRange(start, start + needle.size).contentEquals(needle) }
private fun assertGolden(expected: ByteArray, actual: ByteArray) {
    if (!expected.contentEquals(actual)) throw AssertionError("expected=${expected.hex()} actual=${actual.hex()}")
}
private fun ByteArray.hex(): String = joinToString("") { "%02x".format(it.toInt() and 0xff) }
