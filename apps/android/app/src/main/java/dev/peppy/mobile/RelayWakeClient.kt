package dev.peppy.mobile

import android.content.Context
import android.content.SharedPreferences
import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

/** Content-free relay enrollment. It is deliberately inert until the host enables it. */
data class RelayRoute(val routeId: String, val manageCredential: String, val wakeCredential: String)

interface RelayWakeTransport {
    @Throws(IOException::class) fun post(path: String, body: String): HttpResult
}

fun interface RelayRoutePublisher { @Throws(IOException::class) fun publish(routeId: String, wakeCredential: String) }
interface RelayRouteStore {
    fun route(): RelayRoute?
    fun save(route: RelayRoute)
    fun clear()
    fun pendingRegistration(): String?
    fun savePendingRegistration(id: String)
    fun clearPendingRegistration()
}

/** Keystore-wrapped route credentials. Provider tokens and manage credentials never leave this store. */
class RelayCredentialStore(private val preferences: SharedPreferences, private val secrets: SecretBox = KeystoreSecretBox) : RelayRouteStore {
    override fun route(): RelayRoute? = preferences.getString(KEY, null)?.let { sealed ->
        secrets.open(PURPOSE, sealed)?.let { bytes ->
            try {
                val value = JSONObject(String(bytes, Charsets.UTF_8))
                RelayRoute(value.getString("route_id"), value.getString("manage_credential"), value.getString("wake_credential"))
            } catch (_: Exception) { null }
        }
    }

    override fun save(route: RelayRoute) {
        val body = JSONObject().put("route_id", route.routeId).put("manage_credential", route.manageCredential)
            .put("wake_credential", route.wakeCredential).toString().toByteArray(Charsets.UTF_8)
        preferences.edit().putString(KEY, secrets.seal(PURPOSE, body)).apply()
    }

    override fun clear() { preferences.edit().remove(KEY).apply() }
    override fun pendingRegistration(): String? = preferences.getString(PENDING_KEY, null)?.let { secrets.open(PENDING_PURPOSE, it) }
        ?.toString(Charsets.UTF_8)?.takeIf { UUID_RE.matches(it) }
    override fun savePendingRegistration(id: String) { preferences.edit().putString(PENDING_KEY, secrets.seal(PENDING_PURPOSE, id.toByteArray())).apply() }
    override fun clearPendingRegistration() { preferences.edit().remove(PENDING_KEY).apply() }
    private companion object {
        const val KEY = "relay-route-v1"; const val PURPOSE = "relay-route-v1"
        const val PENDING_KEY = "relay-pending-v1"; const val PENDING_PURPOSE = "relay-pending-v1" // gitleaks:allow -- preference key and encryption-purpose labels, not credentials
        val UUID_RE = Regex("[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", RegexOption.IGNORE_CASE)
    }
}

class RelayWakeClient(
    private val transport: RelayWakeTransport,
    private val credentials: RelayRouteStore,
    private val publisher: RelayRoutePublisher,
    private val provider: String = "fcm",
) {
    /** Starts the push challenge. The caller must pass its provider token; it is never persisted here. */
    fun register(deviceToken: String): String? {
        if (!validToken(deviceToken)) return null
        val response = transport.post("/v1/registrations", JSONObject().put("provider", provider).put("device_token", deviceToken).toString())
        if (response.code != 201) return null
        return try { JSONObject(requireNotNull(response.body)).getString("registration_id").takeIf { UUID_RE.matches(it) }?.also { credentials.savePendingRegistration(it) } } catch (_: Exception) { null }
    }

    /** Called only after the provider delivered the opaque challenge. */
    fun confirm(challenge: String, installationPublicIdentity: String? = null): Boolean {
        val registrationId = credentials.pendingRegistration() ?: return false
        if (!UUID_RE.matches(registrationId) || !SECRET_RE.matches(challenge)) return false
        val body = JSONObject().put("challenge", challenge).also { if (installationPublicIdentity != null) it.put("installation_public_identity", installationPublicIdentity) }
        val response = transport.post("/v1/registrations/$registrationId/confirm", body.toString())
        if (response.code != 201) return false
        val route = try {
            val value = JSONObject(requireNotNull(response.body))
            RelayRoute(value.getString("route_id"), value.getString("manage_credential"), value.getString("wake_credential"))
        } catch (_: Exception) { return false }
        credentials.save(route)
        try { publisher.publish(route.routeId, route.wakeCredential) } catch (_: IOException) { return false }
        credentials.clearPendingRegistration()
        return true
    }

    private fun validToken(token: String) = token.isNotBlank() && token.toByteArray(Charsets.UTF_8).size <= 4096
    private companion object {
        val UUID_RE = Regex("[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", RegexOption.IGNORE_CASE)
        val SECRET_RE = Regex("[0-9a-f]{64}", RegexOption.IGNORE_CASE)
    }
}

/** Fixed-origin, redirect-refusing relay transport; relay endpoints are unauthenticated by design. */
class AndroidRelayWakeTransport(origin: String) : RelayWakeTransport {
    private val origin = requireNotNull(CredentialParser.canonicalOrigin(origin, false))
    override fun post(path: String, body: String): HttpResult {
        require(path.startsWith("/v1/") && !path.contains('?') && !path.contains('#') && !path.contains("//"))
        val connection = URL(origin + path).openConnection() as HttpURLConnection
        try {
            val bytes = body.toByteArray(Charsets.UTF_8)
            connection.instanceFollowRedirects = false; connection.useCaches = false; connection.requestMethod = "POST"
            connection.connectTimeout = GatewayHttp.TIMEOUT_MS; connection.readTimeout = GatewayHttp.TIMEOUT_MS
            connection.setRequestProperty("Content-Type", "application/json"); connection.setRequestProperty("Accept", "application/json")
            connection.doOutput = true; connection.setFixedLengthStreamingMode(bytes.size)
            connection.outputStream.use { it.write(bytes) }
            val code = connection.responseCode
            val stream = if (code in 200..299) connection.inputStream else connection.errorStream
            return HttpResult(code, stream?.use { GatewayHttp.readBounded(it, GatewayHttp.MAX_ERROR_BYTES) })
        } finally { connection.disconnect() }
    }
}

/** Host-owned injection point; unset means relay enrollment and wakes are disabled. */
object RelayWakeRuntime {
    @Volatile var enabled: Boolean = false
    @Volatile var client: RelayWakeClient? = null

    private const val PREFS = "peppy-relay"
    private const val ENABLED = "relay-enabled.v1"
    private const val ORIGIN = "relay-origin.v1"

    /** Rebuild volatile transport state after process death; secrets stay in [RelayCredentialStore]. */
    fun initialize(context: Context) {
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val origin = prefs.getString(ORIGIN, null)?.let { CredentialParser.canonicalOrigin(it, false) }
        if (!prefs.getBoolean(ENABLED, false) || origin == null) {
            enabled = false; client = null; return
        }
        client = RelayWakeClient(AndroidRelayWakeTransport(origin), RelayCredentialStore(prefs),
            RelayRoutePublisher { routeId, wakeCredential -> GatewayAccountHost.publishWake(context, routeId, wakeCredential) })
        enabled = true
    }

    fun setEnabled(context: Context, origin: String?, value: Boolean) {
        val canonical = origin?.let { CredentialParser.canonicalOrigin(it, false) }
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val editor = prefs.edit().putBoolean(ENABLED, value && canonical != null)
        if (canonical == null) editor.remove(ORIGIN) else editor.putString(ORIGIN, canonical)
        editor.apply()
        initialize(context)
    }
}
