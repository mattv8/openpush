package dev.peppy.mobile

import android.content.Context
import org.json.JSONObject
import uniffi.peppy_mobile_bindings.generateNativeEnrollmentKey
import uniffi.peppy_mobile_bindings.pairingProofBytes
import java.net.HttpURLConnection
import java.net.URI
import java.net.URL
import java.util.Locale
import java.util.UUID

/** Native-only QR enrollment state; no secret is ever returned to Compose state. */
internal data class PairingPayload(val origin: String, val intentToken: String)
internal data class PairingClaim(val deviceId: String, val keyDigest: String, val sas: String)

internal object PairingHost {
    private const val PREFS = "peppy-pairing"
    private const val SEED = "enrollment-seed.v1"
    private const val CLAIM = "claim-secret.v1"
    private const val INTENT = "intent-token.v1"
    private const val DEVICE = "device-id.v1"
    private const val DIGEST = "key-digest.v1"
    private val token = Regex("[A-Za-z0-9_-]{43}")
    private val digest = Regex("[0-9a-f]{64}")

    fun parseQr(bytes: String): PairingPayload? = try {
        val value = JSONObject(bytes)
        if (value.length() != 2) null else {
            val origin = canonicalPairingOrigin(value.getString("https_origin"))
            val intent = value.getString("intent_token").takeIf { token.matches(it) }
            if (origin == null || intent == null) null else PairingPayload(origin, intent)
        }
    } catch (_: Exception) { null }

    /** Server pairing origins permit an optional root slash, but no other URL components. */
    private fun canonicalPairingOrigin(raw: String): String? {
        val uri = URI(raw)
        val host = uri.host?.lowercase(Locale.ROOT) ?: return null
        if (uri.scheme?.lowercase(Locale.ROOT) != "https" || uri.rawUserInfo != null ||
            uri.rawQuery != null || uri.rawFragment != null || uri.rawPath !in listOf(null, "", "/") ||
            uri.port !in -1..65535
        ) return null
        val authorityHost = if (':' in host && !host.startsWith('[')) "[$host]" else host
        return if (uri.port == -1) "https://$authorityHost" else "https://$authorityHost:${uri.port}"
    }

    /** Claims once and persists only Keystore-wrapped key material and the claim retrieval secret. */
    fun claim(context: Context, payload: PairingPayload): PairingClaim? {
        val key = generateNativeEnrollmentKey()
        val seed = key.exportSeedForNativeSecureStorage()
        if (seed.size != 32) { seed.fill(0); key.close(); return null }
        return try {
            val device = UUID.randomUUID().toString()
            val body = JSONObject().put("device_id", device)
                .put("public_key", JSONObject().put("ed25519_public_key", key.publicKeyBase64url()))
                .put("requested_role", "gateway").toString()
            val response = anonymous(payload.origin, "POST", "/v1/pairing/intents/${payload.intentToken}/claim", body) ?: return null
            val json = JSONObject(response)
            val keyDigest = json.getString("key_digest").takeIf { digest.matches(it) } ?: return null
            val sas = json.getString("sas").takeIf { it.matches(Regex("[0-9]{6}")) } ?: return null
            val secret = json.getString("claim_secret").takeIf { token.matches(it) } ?: return null
            // The server is not authoritative for the verification code.  Bind it to our own key
            // and exact intent/device tuple before storing any claim material.
            if (key.pairingSas(payload.intentToken, device, keyDigest) != sas) return null
            val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            val editor = prefs.edit()
                .putString(SEED, KeystoreSecretBox.seal(SEED, seed))
                .putString(CLAIM, KeystoreSecretBox.seal(CLAIM, secret.toByteArray()))
                .putString(INTENT, payload.intentToken).putString(DEVICE, device).putString(DIGEST, keyDigest)
            if (!editor.commit()) return null
            PairingClaim(device, keyDigest, sas)
        } finally { seed.fill(0); key.close() }
    }

    /** Retrieves only an owner-approved challenge, signs canonical Rust bytes, consumes it, then imports its credential. */
    fun finishApproved(context: Context, payload: PairingPayload): ImportResult? {
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val device = prefs.getString(DEVICE, null) ?: return null
        val keyDigest = prefs.getString(DIGEST, null)?.takeIf { digest.matches(it) } ?: return null
        val secret = prefs.getString(CLAIM, null)?.let { KeystoreSecretBox.open(CLAIM, it) } ?: return null
        val seed = prefs.getString(SEED, null)?.let { KeystoreSecretBox.open(SEED, it) } ?: return null
        return try {
            if (prefs.getString(INTENT, null) != payload.intentToken || seed.size != 32) return null
            val retrieve = JSONObject().put("device_id", device).put("key_digest", keyDigest)
                .put("claim_secret", String(secret)).toString()
            val challenge = anonymous(payload.origin, "POST", "/v1/pairing/intents/${payload.intentToken}/challenge", retrieve) ?: return null
            val c = JSONObject(challenge)
            val challengeToken = c.getString("challenge_token").takeIf { token.matches(it) } ?: return null
            val vault = c.getString("vault_id")
            if (!NativeGateway.canImportIdentity(context, payload.origin, vault, device)) return null
            val fingerprint = c.getString("profile_fingerprint").takeIf { digest.matches(it) } ?: return null
            val epoch = c.getLong("key_epoch").takeIf { it in 0..0xffff_ffffL }?.toUInt() ?: return null
            val role = c.getString("requested_role").takeIf { it == "gateway" || it == "device" } ?: return null
            val key = uniffi.peppy_mobile_bindings.nativeEnrollmentKeyFromNativeSecureStorage(seed)
            val publicKey = key.publicKeyBase64url()
            val proof = pairingProofBytes(challengeToken, vault, device, fingerprint, epoch, role)
            val signature = key.signPairingProof(proof)
            proof.fill(0); key.close()
            val consume = JSONObject().put("challenge_token", challengeToken).put("device_id", device)
                .put("public_key", JSONObject().put("ed25519_public_key", publicKey))
                .put("profile_fingerprint", fingerprint).put("key_epoch", epoch.toLong()).put("signature", signature).toString()
            val credential = JSONObject(anonymous(payload.origin, "POST", "/v1/pairing/consume", consume) ?: return null)
            val file = JSONObject().put("version", 1).put("origin", payload.origin).put("vaultId", credential.getString("vault_id"))
                .put("deviceId", credential.getString("device_id")).put("deviceToken", credential.getString("device_token")).toString().toByteArray()
            NativeGateway.importCredential(context, file).also { if (it == ImportResult.IMPORTED || it == ImportResult.UPDATED) prefs.edit().clear().commit() }
        } finally { seed.fill(0); secret.fill(0) }
    }

    /** Cancellation must not leave a phone-owned enrollment key reusable by a later QR code. */
    fun cancel(context: Context) {
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().clear().commit()
    }

    private fun anonymous(origin: String, method: String, path: String, body: String): String? {
        val connection = URL(origin + path).openConnection() as HttpURLConnection
        return try {
            connection.instanceFollowRedirects = false; connection.connectTimeout = 10_000; connection.readTimeout = 10_000
            connection.requestMethod = method; connection.setRequestProperty("Content-Type", "application/json"); connection.doOutput = true
            connection.outputStream.use { it.write(body.toByteArray()) }
            if (connection.responseCode !in 200..299) null else connection.inputStream.use { GatewayHttp.readBounded(it, 64 * 1024) }
        } catch (_: Exception) { null } finally { connection.disconnect() }
    }
}
