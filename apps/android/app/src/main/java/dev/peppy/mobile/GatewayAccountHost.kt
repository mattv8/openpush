package dev.peppy.mobile

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL

internal data class GatewayAccountDevice(val id: String, val role: String, val revoked: Boolean, val keyEpoch: Long)
internal data class GatewayAccountState(val self: GatewayAccountDevice, val devices: List<GatewayAccountDevice>)

/** Authenticated account boundary. Credentials stay inside NativeGateway / GatewayHttp. */
internal object GatewayAccountHost {
    fun load(context: Context): GatewayAccountState? {
        val credential = NativeGateway.accountCredential(context) ?: return null
        val result = try { GatewayHttp(credential.origin, credential.bearerToken).get("/v1/devices") } catch (_: Exception) { return null }
        if (!result.ok) return null
        return try {
            val list = JSONObject(result.body ?: return null).getJSONArray("devices").devices()
            val self = list.firstOrNull { it.id == credential.deviceId } ?: return null
            GatewayAccountState(self, list)
        } catch (_: Exception) { null }
    }

    fun revoke(context: Context, target: String): Boolean {
        val credential = NativeGateway.accountCredential(context) ?: return false
        if (!UUID_RE.matches(target)) return false
        val result = try { GatewayHttp(credential.origin, credential.bearerToken).postJson("/v1/devices/$target/revoke", "{}") } catch (_: Exception) { return false }
        // A credential can be replaced while the request is in flight.  Never archive the newer
        // enrollment merely because an earlier identity successfully revoked itself.
        val stillCurrent = NativeGateway.accountCredential(context)
        return if (target == credential.deviceId) {
            val expectedIdentity = stillCurrent?.let { it.deviceId == credential.deviceId && it.bearerToken == credential.bearerToken } == true
            // A self-revoke can commit before a connection drops.  A now-invalid token (401) or
            // already-revoked conflict (409) is evidence sufficient to stop this exact identity;
            // do not apply that inference to another device's revoke request.
            expectedIdentity && (result.ok || result.code == 401 || result.code == 409) && NativeGateway.archiveEnrollment(context)
        } else result.ok
    }

    fun deleteVault(context: Context): Boolean {
        val credential = NativeGateway.accountCredential(context) ?: return false
        val result = try { GatewayHttp(credential.origin, credential.bearerToken).deleteJson("/v1/vault", JSONObject().put("vault_id", credential.vaultId).toString()) } catch (_: Exception) { return false }
        val stillCurrent = NativeGateway.accountCredential(context)
        return result.ok && stillCurrent?.let { it.deviceId == credential.deviceId && it.bearerToken == credential.bearerToken } == true &&
            NativeGateway.archiveEnrollment(context)
    }

    fun publishWake(context: Context, routeId: String, wakeCredential: String) {
        val credential = NativeGateway.accountCredential(context) ?: throw java.io.IOException("not enrolled")
        val connection = URL(credential.origin + "/v1/devices/self/wake-route").openConnection() as HttpURLConnection
        try {
            val body = JSONObject().put("route_id", routeId).put("wake_credential", wakeCredential).toString().toByteArray()
            connection.instanceFollowRedirects = false; connection.requestMethod = "PUT"; connection.connectTimeout = GatewayHttp.TIMEOUT_MS; connection.readTimeout = GatewayHttp.TIMEOUT_MS
            connection.setRequestProperty("Authorization", "Bearer ${credential.bearerToken}"); connection.setRequestProperty("Content-Type", "application/json"); connection.doOutput = true
            connection.outputStream.use { it.write(body) }
            if (connection.responseCode !in 200..299) throw java.io.IOException("route publisher rejected")
        } finally { connection.disconnect() }
    }

    private fun JSONArray.devices() = buildList {
        for (index in 0 until length()) {
            val item = getJSONObject(index); val id = item.getString("device_id")
            val role = item.getString("role").takeIf { it == "owner" || it == "gateway" || it == "device" } ?: continue
            if (!UUID_RE.matches(id)) continue
            add(GatewayAccountDevice(id, role, item.optBoolean("revoked"), item.optLong("key_epoch", 0)))
        }
    }
    private val UUID_RE = Regex("[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", RegexOption.IGNORE_CASE)
}
