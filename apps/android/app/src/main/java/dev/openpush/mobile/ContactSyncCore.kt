package dev.openpush.mobile

import dev.openpush.mobile.AndroidContactsProvider.FieldOp
import dev.openpush.mobile.AndroidContactsProvider.NewField
import dev.openpush.mobile.AndroidContactsProvider.PhotoOp
import dev.openpush.mobile.AndroidContactsProvider.WriteResult
import org.json.JSONObject
import uniffi.openpush_mobile_bindings.MobileBindingsException
import uniffi.openpush_mobile_bindings.NativeAttachmentState
import uniffi.openpush_mobile_bindings.NativeClientInterface
import java.io.File

/**
 * The Rust core's contact surface used by the Android host. Every call is a method on the one
 * already-open [NativeClientInterface]; requests and responses are the core's JSON schemas.
 * The core owns books, policy, default account, IDs, revisions, scan checkpoints, permits and
 * apply evidence; the host keeps no parallel copy of any of them.
 */
internal interface ContactCore {
    fun listBooks(): JSONObject
    fun capture(input: JSONObject): JSONObject
    fun beginScan(input: JSONObject): JSONObject
    fun observeScan(input: JSONObject): JSONObject
    fun finishScan(input: JSONObject): JSONObject
    fun scanState(input: JSONObject): JSONObject
    fun listRequests(input: JSONObject): JSONObject
    fun nextPermit(input: JSONObject): JSONObject
    fun applyEvidence(input: JSONObject): JSONObject
    fun reconcile(input: JSONObject): JSONObject
    fun settings(input: JSONObject): JSONObject
    fun approval(input: JSONObject): JSONObject
    fun bookView(input: JSONObject): JSONObject
    /** Owner-only source context by `contact_id` or `source_key`; null when unknown. */
    fun sourceContext(input: JSONObject): JSONObject?

    /** Normalizes and encrypts a photo file; returns the tracked attachment ID. */
    fun preparePhoto(path: String): String
    /** Attachment state, or null when the core has no such attachment. */
    fun attachmentState(attachmentId: String): NativeAttachmentState?
    /** Plaintext bytes of an installed (download-verified) attachment, bounded. */
    fun attachmentBytes(attachmentId: String, maxBytes: Int): ByteArray?
}

internal class NativeContactCore(private val client: NativeClientInterface) : ContactCore {
    override fun listBooks() = JSONObject(client.listContactBooksJson())
    override fun capture(input: JSONObject) = JSONObject(client.capturePlatformContactsJson(input.toString()))
    override fun beginScan(input: JSONObject) = JSONObject(client.beginContactScan(input.toString()))
    override fun observeScan(input: JSONObject) = JSONObject(client.observeContactScan(input.toString()))
    override fun finishScan(input: JSONObject) = JSONObject(client.finishContactScan(input.toString()))
    override fun scanState(input: JSONObject) = JSONObject(client.contactScanStateJson(input.toString()))
    override fun listRequests(input: JSONObject) = JSONObject(client.listContactRequestsJson(input.toString()))
    override fun nextPermit(input: JSONObject) = JSONObject(client.nextContactApplyPermit(input.toString()))
    override fun applyEvidence(input: JSONObject) = JSONObject(client.contactApplyEvidenceJson(input.toString()))
    override fun reconcile(input: JSONObject) = JSONObject(client.reconcileContactApply(input.toString()))
    override fun settings(input: JSONObject) = JSONObject(client.contactSettingsJson(input.toString()))
    override fun approval(input: JSONObject) = JSONObject(client.contactApprovalJson(input.toString()))
    override fun bookView(input: JSONObject) = JSONObject(client.contactBookView(input.toString()))
    override fun sourceContext(input: JSONObject): JSONObject? = try {
        JSONObject(client.contactSourceContextJson(input.toString()))
    } catch (_: MobileBindingsException.NotFound) {
        null
    }
    override fun preparePhoto(path: String): String = client.prepareContactPhoto(path).attachmentId

    override fun attachmentState(attachmentId: String): NativeAttachmentState? = try {
        client.attachmentInfo(attachmentId).state
    } catch (_: MobileBindingsException.NotFound) {
        null
    }

    override fun attachmentBytes(attachmentId: String, maxBytes: Int): ByteArray? {
        if (attachmentState(attachmentId) != NativeAttachmentState.AVAILABLE) return null
        val handle = client.openNativePlaintextFile(attachmentId)
        return try {
            val file = File(handle.nativePlaintextPath())
            if (!file.isFile || file.length() > maxBytes) null else file.readBytes()
        } finally {
            handle.dispose()
        }
    }
}

/** The OS contacts surface the coordinator needs; [AndroidContactsProvider] in production. */
internal interface ContactsOs {
    fun accounts(): List<ContactAccount>
    fun osDefaultAccount(): AndroidContactsProvider.DefaultAccount
    fun scan(afterContactId: Long?, limit: Int, updatedSince: Long?): AndroidContactsProvider.Page
    fun contactByLookup(lookupKey: String): AndroidContactsProvider.Lookup
    fun contactByRawContact(rawContactId: Long): AndroidContactsProvider.Lookup
    fun deletedSince(sinceMillis: Long): Int?
    fun readPhoto(contactId: Long): AndroidContactsProvider.PhotoRead
    fun update(rawContactId: Long, ops: List<FieldOp>, photo: PhotoOp?): WriteResult
    fun create(account: ContactAccount?, fields: List<NewField>, photo: ByteArray?): WriteResult
    fun delete(rawContactId: Long, expectedVersion: Int): WriteResult
    fun findCreateMatches(account: ContactAccount?, fields: List<NewField>): Set<Long>?
}

internal class AndroidContactsOs(private val provider: AndroidContactsProvider) : ContactsOs {
    override fun accounts() = provider.accounts()
    override fun osDefaultAccount() = provider.osDefaultAccount()
    override fun scan(afterContactId: Long?, limit: Int, updatedSince: Long?) = provider.scan(afterContactId, limit, updatedSince)
    override fun contactByLookup(lookupKey: String) = provider.contactByLookup(lookupKey)
    override fun contactByRawContact(rawContactId: Long) = provider.contactByRawContact(rawContactId)
    override fun deletedSince(sinceMillis: Long) = provider.deletedSince(sinceMillis)
    override fun readPhoto(contactId: Long) = provider.readPhoto(contactId)
    override fun update(rawContactId: Long, ops: List<FieldOp>, photo: PhotoOp?) = provider.update(rawContactId, ops, photo)
    override fun create(account: ContactAccount?, fields: List<NewField>, photo: ByteArray?) = provider.create(account, fields, photo)
    override fun delete(rawContactId: Long, expectedVersion: Int) = provider.delete(rawContactId, expectedVersion)
    override fun findCreateMatches(account: ContactAccount?, fields: List<NewField>) = provider.findCreateMatches(account, fields)
}
