package dev.peppy.mobile

import org.json.JSONObject
import uniffi.peppy_mobile_bindings.NativeClientInterface
import java.util.UUID

/**
 * Contact photo server bookkeeping around the shared media queue:
 *  - which pending uploads must be reserved with `reference_tracking`;
 *  - registering every sealed photo-bearing envelope's references BEFORE the core releases the
 *    envelope for upload (core withholds it until acknowledged);
 *  - reclaim of tracked photos the core no longer references, proven by the server against the
 *    current compaction generation and replay floor read fresh from `GET /v1/snapshot`.
 */
class ContactPhotoTransfer(private val client: NativeClientInterface, private val http: GatewayHttp) {
    fun trackedUploadIds(): Set<String> {
        val uploads = JSONObject(client.contactPhotoTransferStateJson()).optJSONArray("uploads") ?: return emptySet()
        return (0 until uploads.length()).map { uploads.getJSONObject(it) }
            .filter { it.optBoolean("reference_tracking") }
            .mapTo(HashSet()) { it.getString("attachment_id") }
    }

    /** Returns true when more registration/reclaim work remains for a follow-up pass. */
    fun run(): Boolean {
        val state = JSONObject(client.contactPhotoTransferStateJson())
        val registrations = state.optJSONArray("registrations")
        var more = false
        var transient: Exception? = null
        if (registrations != null) {
            for (index in 0 until minOf(registrations.length(), MAX_REGISTRATIONS)) {
                val row = registrations.getJSONObject(index)
                try {
                    register(row)
                } catch (error: GatewayAuthException) {
                    throw error
                } catch (error: GatewayTransportException) {
                    if (transient == null) transient = error
                }
            }
            more = registrations.length() > MAX_REGISTRATIONS
        }
        val reclaims = state.optJSONArray("reclaims")
        if (reclaims != null && reclaims.length() > 0) {
            val fence = snapshotFence()
            if (fence != null) {
                for (index in 0 until minOf(reclaims.length(), MAX_RECLAIMS)) {
                    try {
                        reclaim(reclaims.getJSONObject(index), fence)
                    } catch (error: GatewayAuthException) {
                        throw error
                    } catch (error: GatewayTransportException) {
                        if (transient == null) transient = error
                    }
                }
                more = more || reclaims.length() > MAX_RECLAIMS
            }
        }
        transient?.let { throw it }
        return more
    }

    private fun register(row: JSONObject) {
        val remote = UUID.fromString(row.getString("attachment_id")).toString()
        val body = JSONObject().put(
            "references",
            org.json.JSONArray().put(
                JSONObject().put("producer_device_id", row.getString("producer_device_id")).put("producer_sequence", row.getString("producer_sequence")),
            ),
        )
        val response = http.postJson("/v1/attachments/$remote/references", body.toString())
        when {
            response.ok -> client.acknowledgeContactPhotoReferenceJson(
                JSONObject().put("schema_version", 1).put("envelope_id", row.getString("envelope_id")).put("attachment_id", remote).toString(),
            )
            response.code == 401 || response.code == 403 -> throw GatewayAuthException()
            // Untracked upload: the envelope stays held (never published unregistered); visible as stuck work.
            response.code == 409 -> Unit
            else -> throw GatewayTransportException("reference registration HTTP ${response.code}")
        }
    }

    private class Fence(val generation: String, val replayFloor: ULong)

    private fun snapshotFence(): Fence? {
        val response = http.get("/v1/snapshot")
        if (response.code == 401 || response.code == 403) throw GatewayAuthException()
        if (!response.ok) throw GatewayTransportException("snapshot HTTP ${response.code}")
        val start = JSONObject(response.body ?: throw GatewayTransportException("empty body"))
        if (start.opt("compaction_supported") != true) return null
        val generation = start.optString("compaction_generation").takeIf { it.toULongOrNull()?.toString() == it } ?: return null
        val floor = start.optString("replay_floor_cursor").takeIf { it.toULongOrNull()?.toString() == it }?.toULong() ?: return null
        return Fence(generation, floor)
    }

    private fun reclaim(row: JSONObject, fence: Fence) {
        // The server can only prove release once every own reference was echoed back and the
        // replay floor has passed it; earlier DELETEs would just 409 and back off.
        val releaseAfter = row.optString("release_after_cursor").toULongOrNull() ?: return
        if (fence.replayFloor < releaseAfter) return
        val remote = UUID.fromString(row.getString("remote_object_id")).toString()
        val body = JSONObject().put("compaction_generation", fence.generation).put("release_before_cursor", fence.replayFloor.toString())
        val response = http.deleteJson("/v1/attachments/$remote", body.toString())
        when (response.code) {
            204, 404, 409 -> client.acknowledgeContactPhotoReclaimJson(
                JSONObject().put("schema_version", 1).put("attachment_id", row.getString("attachment_id")).put("http_status", response.code).toString(),
            )
            401, 403 -> throw GatewayAuthException()
            else -> throw GatewayTransportException("reclaim HTTP ${response.code}")
        }
    }

    private companion object {
        const val MAX_REGISTRATIONS = 20
        const val MAX_RECLAIMS = 10
    }
}
