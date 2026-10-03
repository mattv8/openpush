package dev.peppy.mobile

import android.util.JsonReader
import android.util.JsonToken
import java.io.IOException
import java.io.StringReader
import java.net.URI
import java.net.URISyntaxException
import java.util.Locale
import java.util.UUID

/**
 * A v1 paired-device credential file, the same strict format the desktop imports. It identifies an
 * existing vault device; it never contains or creates a vault passphrase.
 */
class DeviceCredential internal constructor(
    val origin: String,
    val vaultId: String,
    val deviceId: String,
    /** Bearer token. Never logged or shown. */
    internal val deviceToken: String,
) {
    override fun toString() = "DeviceCredential(origin=$origin, vaultId=$vaultId, deviceId=$deviceId)"
}

object CredentialParser {
    const val MAX_BYTES = 16 * 1024
    private val FIELDS = setOf("version", "origin", "vaultId", "deviceId", "deviceToken")
    private const val TOKEN_HEX_CHARS = 96

    /**
     * Bounded, strict parse. Rejects duplicate or unknown members, non-string values, any version
     * other than the JSON integer literal `1`, and trailing content. Never echoes the input.
     */
    fun parse(bytes: ByteArray, allowDebugLoopback: Boolean = BuildConfig.DEBUG): DeviceCredential? {
        if (bytes.size > MAX_BYTES) return null
        val values = HashMap<String, String>()
        try {
            JsonReader(StringReader(String(bytes, Charsets.UTF_8))).use { reader ->
                reader.isLenient = false
                reader.beginObject()
                while (reader.hasNext()) {
                    val name = reader.nextName()
                    if (name !in FIELDS || name in values) return null
                    val expected = if (name == "version") JsonToken.NUMBER else JsonToken.STRING
                    if (reader.peek() != expected) return null
                    values[name] = reader.nextString()
                }
                reader.endObject()
                if (reader.peek() != JsonToken.END_DOCUMENT) return null
            }
        } catch (_: IOException) {
            return null
        } catch (_: IllegalStateException) {
            return null
        } catch (_: NumberFormatException) {
            return null
        }
        if (values.keys != FIELDS || values["version"] != "1") return null
        val token = values.getValue("deviceToken")
        if (token.length != TOKEN_HEX_CHARS || !token.all { it in '0'..'9' || it in 'a'..'f' || it in 'A'..'F' }) return null
        return DeviceCredential(
            origin = canonicalOrigin(values.getValue("origin"), allowDebugLoopback) ?: return null,
            vaultId = canonicalUuid(values.getValue("vaultId")) ?: return null,
            deviceId = canonicalUuid(values.getValue("deviceId")) ?: return null,
            deviceToken = token,
        )
    }

    /**
     * `https://host[:port]` only, with no path, query, fragment, or user info. Cleartext HTTP is
     * accepted only for loopback/emulator hosts in debug builds.
     */
    fun canonicalOrigin(raw: String, allowDebugLoopback: Boolean = BuildConfig.DEBUG): String? = try {
        val uri = URI(raw.trimEnd('/'))
        val scheme = uri.scheme?.lowercase(Locale.ROOT)
        val host = uri.host?.lowercase(Locale.ROOT)
        val cleartextAllowed = allowDebugLoopback && host in DEBUG_LOOPBACK_HOSTS
        when {
            host.isNullOrEmpty() || uri.rawUserInfo != null || uri.rawQuery != null || uri.rawFragment != null -> null
            !uri.rawPath.isNullOrEmpty() -> null
            scheme == "https" || (scheme == "http" && cleartextAllowed) ->
                if (uri.port == -1) "$scheme://$host" else "$scheme://$host:${uri.port}"
            else -> null
        }
    } catch (_: URISyntaxException) {
        null
    }

    private fun canonicalUuid(raw: String): String? = try {
        // UUID.fromString accepts abbreviated groups; require the canonical 36-character form.
        UUID.fromString(raw).toString().takeIf { it == raw.lowercase(Locale.ROOT) }
    } catch (_: IllegalArgumentException) {
        null
    }

    private val DEBUG_LOOPBACK_HOSTS = setOf("127.0.0.1", "localhost", "10.0.2.2")
}

/** Public vault material returned by `GET /v1/vault` for this device's bearer token. */
class VaultMaterial(val profileJson: String, val headerJson: String)

/** Strict u32 from a JSON integer member: rejects strings, fractions, negatives, and overflow. */
internal fun org.json.JSONObject.strictUInt(name: String): UInt? = when (val value = opt(name)) {
    is Int -> value.takeIf { it >= 0 }?.toUInt()
    is Long -> value.takeIf { it in 0L..0xFFFF_FFFFL }?.toUInt()
    else -> null
}
