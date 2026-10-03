/*
 * SPDX-License-Identifier: Apache-2.0
 *
 * Adapted from the bounded encoding routines in AOSP Messaging mmslib/pdu/
 * PduComposer.java (pinned source and section mapping in NOTICE). The Android
 * model, persister, resolver, APN and network code are intentionally absent.
 */
package dev.peppy.mobile.mms.pdu

import java.io.BufferedOutputStream
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.IOException
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.util.Locale

/** A bounded M-Send.req composer for Android's public SmsManager hand-off. */
object MmsPduComposer {
    const val MAX_RECIPIENTS = 20
    const val MAX_PARTS = 10
    const val MAX_PART_BYTES = 32L * 1024 * 1024
    const val MAX_TEXT_BYTES = 1024 * 1024
    const val MAX_SUBJECT_BYTES = 1024
    const val MAX_PDU_BYTES = 32L * 1024 * 1024
    private val phoneAddress = Regex("^[+]?[0-9*#.-]+$")
    private val emailAddress = Regex("^[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Za-z0-9](?:[A-Za-z0-9.-]*[A-Za-z0-9])?$")
    private val mediaType = Regex("^[A-Za-z0-9!#$&^_.+-]+/[A-Za-z0-9!#$&^_.+-]+$")
    /** Test-only deterministic seam for verifying external source-size mutation handling. */
    @Volatile internal var afterPartSizeReadForTest: ((File) -> Unit)? = null

    @Throws(MmsPduException::class)
    fun compose(request: MmsPduRequest): MmsPduResult {
        val normalized = validateAndNormalize(request)
        val destination = request.outputFile.absoluteFile
        val parent = destination.parentFile ?: throw MmsPduValidationException("output file has no parent")
        if (!parent.isDirectory) throw MmsPduValidationException("output directory does not exist")
        var temporary: File? = null
        try {
            temporary = File.createTempFile(".mms-pdu-", ".tmp", parent)
            val raw = FileOutputStream(temporary)
            val output = CountingOutput(BufferedOutputStream(raw), request.maximumPduBytes)
            val size = try {
                writeHeaders(output, request, normalized)
                writeUintvar(output, request.parts.size + if (normalized.text != null) 1 else 0)
                normalized.text?.let { writeMemoryPart(output, "text/plain", null, null, it.toByteArray(Charsets.UTF_8), true) }
                request.parts.forEach { writeFilePart(output, it) }
                output.flush()
                raw.fd.sync()
                output.count
            } finally {
                output.close()
            }
            Files.move(temporary.toPath(), destination.toPath(), StandardCopyOption.ATOMIC_MOVE, StandardCopyOption.REPLACE_EXISTING)
            temporary = null
            return MmsPduResult(size)
        } catch (error: MmsPduException) {
            throw error
        } catch (error: IOException) {
            throw MmsPduException("failed to compose MMS PDU: ${error.message}", error)
        } catch (error: SecurityException) {
            throw MmsPduException("failed to compose MMS PDU", error)
        } finally {
            temporary?.delete()
        }
    }

    private data class Normalized(val recipients: List<String>, val subject: String?, val text: String?)

    private fun validateAndNormalize(request: MmsPduRequest): Normalized {
        if (request.transactionId.isEmpty() || request.transactionId.toByteArray(Charsets.US_ASCII).size > 64 || request.transactionId.any { it !in '!'..'~' }) {
            throw MmsPduValidationException("transaction ID must be 1..64 printable ASCII bytes")
        }
        if (request.recipients.size !in 1..MAX_RECIPIENTS) throw MmsPduValidationException("recipient count must be 1..$MAX_RECIPIENTS")
        val recipients = request.recipients.map { normalizeAddress(it.address) }
        if (recipients.map { it.lowercase(Locale.ROOT) }.toSet().size != recipients.size) throw MmsPduValidationException("duplicate recipient")
        val subject = request.subject?.takeIf { it.isNotEmpty() }
        subject?.let { validateUtf8(it, "subject", MAX_SUBJECT_BYTES) }
        val text = request.text?.takeIf { it.isNotEmpty() }
        text?.let { validateUtf8(it, "text", MAX_TEXT_BYTES) }
        if (text == null && request.parts.isEmpty()) throw MmsPduValidationException("MMS requires text or a part")
        if (request.parts.size > MAX_PARTS) throw MmsPduValidationException("part count exceeds $MAX_PARTS")
        request.parts.forEach { part ->
            if (!part.file.isFile) throw MmsPduValidationException("part is not a regular file")
            if (part.mediaType.length > 255 || !mediaType.matches(part.mediaType)) throw MmsPduValidationException("invalid media type")
            if (part.mediaType.equals("application/smil", true)) throw MmsPduValidationException("outgoing SMIL is unsupported; multipart/related is not composed")
            part.name?.let { validateFilename(it) }
            part.contentId?.let { validateContentId(it) }
        }
        if (request.maximumPduBytes !in 1..MAX_PDU_BYTES) throw MmsPduValidationException("maximum PDU size must be 1..$MAX_PDU_BYTES")
        return Normalized(recipients, subject, text)
    }

    private fun normalizeAddress(address: String): String = when {
        phoneAddress.matches(address) && address.length <= 64 -> "$address/TYPE=PLMN"
        emailAddress.matches(address) && address.length <= 128 -> address
        else -> throw MmsPduValidationException("recipient must be a phone number or email address")
    }
    private fun validateUtf8(value: String, field: String, limit: Int) {
        if (value.indexOf('\u0000') >= 0 || value.toByteArray(Charsets.UTF_8).size > limit) throw MmsPduValidationException("invalid or oversized $field")
    }
    private fun validateFilename(value: String) {
        validateUtf8(value, "part name", 255)
        if (value == "." || value == ".." || value.contains('/') || value.contains('\\')) throw MmsPduValidationException("part name must be a basename")
    }
    private fun validateContentId(value: String) {
        if (value.isEmpty() || value.length > 255 || value.any { it !in '!'..'~' || it == '<' || it == '>' }) throw MmsPduValidationException("invalid content ID")
    }

    /* AOSP PduComposer.makeSendReq / appendHeader (lines 514-572). */
    private fun writeHeaders(output: CountingOutput, request: MmsPduRequest, normalized: Normalized) {
        output.write(0x8c); output.write(0x80) // X-Mms-Message-Type: m-send-req
        output.write(0x98); writeTextString(output, request.transactionId.toByteArray(Charsets.US_ASCII))
        output.write(0x8d); output.write(0x92) // X-Mms-MMS-Version: 1.2
        output.write(0x89); output.write(0x01); output.write(0x81) // From: value-length + insert-address-token
        request.recipients.zip(normalized.recipients).forEach { (recipient, address) ->
            output.write(if (recipient.kind == MmsRecipientKind.TO) 0x97 else 0x82)
            writeTextString(output, address.toByteArray(Charsets.US_ASCII))
        }
        normalized.subject?.let { output.write(0x96); writeEncodedString(output, it) }
        output.write(0x8a); output.write(0x80) // X-Mms-Message-Class: personal
        output.write(0x84); output.write(0xa3) // Content-Type: multipart/mixed
    }

    private fun writeFilePart(output: CountingOutput, part: MmsFilePart) {
        FileInputStream(part.file).channel.use { channel ->
            val size = channel.size()
            if (size !in 0..MAX_PART_BYTES) throw MmsPduValidationException("oversized part file")
            afterPartSizeReadForTest?.invoke(part.file)
            val headers = partHeaders(part.mediaType, part.name, part.contentId, false)
            writeUintvar(output, headers.size)
            writeUintvar(output, size.toInt())
            output.write(headers)
            copyExactly(channel, output, size)
        }
    }

    private fun writeMemoryPart(output: CountingOutput, type: String, name: String?, contentId: String?, body: ByteArray, utf8: Boolean) {
        val headers = partHeaders(type, name, contentId, utf8)
        writeUintvar(output, headers.size)
        writeUintvar(output, body.size)
        output.write(headers)
        output.write(body)
    }

    /* AOSP PduComposer.makeMessageBody (lines 967-1091), stripped to file sources. */
    private fun partHeaders(type: String, name: String?, contentId: String?, utf8: Boolean): ByteArray {
        val body = ByteArrayOutputStream()
        val content = ByteArrayOutputStream()
        writeTextString(content, type.toByteArray(Charsets.US_ASCII))
        name?.let { content.write(0x85); writeTextString(content, it.toByteArray(Charsets.UTF_8)) }
        if (utf8) { content.write(0x81); content.write(0xea) }
        writeValueLength(body, content.size())
        content.writeTo(body)
        name?.let { body.write(0x8e); writeTextString(body, it.toByteArray(Charsets.UTF_8)) }
        contentId?.let { body.write(0xc0); body.write(0x22); writeTextString(body, "<$it>".toByteArray(Charsets.US_ASCII)) }
        return body.toByteArray()
    }

    /* AOSP PduComposer.appendEncodedString (lines 367-393). */
    private fun writeEncodedString(output: java.io.OutputStream, value: String) {
        val text = textString(value.toByteArray(Charsets.UTF_8))
        writeValueLength(output, 1 + text.size)
        output.write(0xea) // UTF-8 well-known charset as Short-integer
        output.write(text)
    }

    /* AOSP PduComposer.appendValueLength/appendTextString (lines 290-299, 412-427). */
    private fun writeValueLength(output: java.io.OutputStream, value: Int) {
        if (value < 31) output.write(value) else { output.write(0x1f); writeUintvar(output, value) }
    }
    private fun textString(bytes: ByteArray): ByteArray {
        val quote = bytes.isNotEmpty() && (bytes[0].toInt() and 0x80) != 0
        return ByteArray(bytes.size + if (quote) 2 else 1).also { encoded ->
            var offset = 0; if (quote) encoded[offset++] = 0x7f
            bytes.copyInto(encoded, offset); encoded[encoded.lastIndex] = 0
        }
    }
    private fun writeTextString(output: java.io.OutputStream, bytes: ByteArray) = output.write(textString(bytes))
    private fun writeUintvar(output: java.io.OutputStream, value: Int) {
        require(value >= 0); var shift = 0; var number = value
        do { shift += 7; number = number ushr 7 } while (number != 0)
        while (shift > 0) { shift -= 7; output.write(((value ushr shift) and 0x7f) or if (shift > 0) 0x80 else 0) }
    }
    private fun copyExactly(channel: java.nio.channels.FileChannel, output: CountingOutput, size: Long) {
        var remaining = size; val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
        while (remaining > 0) {
            val wanted = minOf(buffer.size.toLong(), remaining).toInt()
            val count = channel.read(java.nio.ByteBuffer.wrap(buffer, 0, wanted))
            if (count <= 0) throw MmsPduException("part file changed while composing")
            output.write(buffer, 0, count); remaining -= count
        }
        if (channel.read(java.nio.ByteBuffer.allocate(1)) != -1) throw MmsPduException("part file changed while composing")
    }
}

data class MmsPduRequest(val transactionId: String, val recipients: List<MmsRecipient>, val subject: String?, val text: String?, val parts: List<MmsFilePart>, val maximumPduBytes: Long, val outputFile: File)
data class MmsRecipient(val address: String, val kind: MmsRecipientKind)
enum class MmsRecipientKind { TO, CC }
data class MmsFilePart(val file: File, val mediaType: String, val name: String?, val contentId: String?)
data class MmsPduResult(val byteSize: Long)
open class MmsPduException(message: String, cause: Throwable? = null) : IOException(message, cause)
class MmsPduValidationException(message: String) : MmsPduException(message)

private class CountingOutput(private val delegate: BufferedOutputStream, private val limit: Long) : java.io.OutputStream() {
    var count = 0L; private set
    override fun write(value: Int) { ensure(1); delegate.write(value); count++ }
    override fun write(bytes: ByteArray, offset: Int, length: Int) { ensure(length.toLong()); delegate.write(bytes, offset, length); count += length }
    override fun flush() = delegate.flush()
    override fun close() = delegate.close()
    private fun ensure(add: Long) { if (count + add > limit) throw MmsPduValidationException("encoded PDU exceeds maximum size") }
}
