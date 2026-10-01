package dev.openpush.mobile

import java.io.ByteArrayOutputStream
import java.io.IOException
import java.io.InputStream
import java.net.HttpURLConnection
import java.net.URL

/** One HTTP response. For non-2xx, [body] is the bounded error body (if any). */
class HttpResult(val code: Int, val body: String?) {
    val ok get() = code in 200..299

    /** The server's `{"code":"..."}` error code, if present and well-formed. */
    val errorCode: String?
        get() = if (ok) null else try {
            body?.let { org.json.JSONObject(it).optString("code").takeIf { code -> code.matches(Regex("[a-z_]{1,64}")) } }
        } catch (_: org.json.JSONException) {
            null
        }
}

/** Thrown for transport failures and over-budget responses; the worker retries with backoff. */
class GatewayTransportException(message: String) : IOException(message)

/**
 * Authenticated HTTP to the credential's canonical origin. Redirects are disabled so the bearer
 * token can never be forwarded elsewhere; response bodies are bounded like the Rust simulator's.
 */
class GatewayHttp(private val origin: String, private val bearerToken: String) {
    fun get(path: String): HttpResult = request("GET", path, null)

    fun postJson(path: String, body: String): HttpResult = request("POST", path, body)

    private fun request(method: String, path: String, body: String?): HttpResult {
        require(path.startsWith("/v1/"))
        val connection = URL(origin + path).openConnection() as HttpURLConnection
        try {
            connection.instanceFollowRedirects = false
            connection.useCaches = false
            connection.requestMethod = method
            connection.connectTimeout = TIMEOUT_MS
            connection.readTimeout = TIMEOUT_MS
            connection.setRequestProperty("Authorization", "Bearer $bearerToken")
            connection.setRequestProperty("Accept", "application/json")
            if (body != null) {
                val bytes = body.toByteArray(Charsets.UTF_8)
                connection.doOutput = true
                connection.setFixedLengthStreamingMode(bytes.size)
                connection.setRequestProperty("Content-Type", "application/json")
                connection.outputStream.use { it.write(bytes) }
            }
            val code = connection.responseCode
            if (code !in 200..299) {
                // Small bounded error body so permanent rejections can be told apart by code.
                val error = try {
                    connection.errorStream?.use { readBounded(it, MAX_ERROR_BYTES) }
                } catch (_: IOException) {
                    null
                }
                return HttpResult(code, error)
            }
            return HttpResult(code, connection.inputStream.use { readBounded(it) })
        } finally {
            connection.disconnect()
        }
    }

    companion object {
        const val TIMEOUT_MS = 10_000
        /** Server payload budget (8 MiB) plus framing allowance. */
        const val MAX_RESPONSE_BYTES = 8 * 1024 * 1024 + 64 * 1024
        const val MAX_ERROR_BYTES = 16 * 1024

        internal fun readBounded(input: InputStream, limit: Int = MAX_RESPONSE_BYTES): String {
            val out = ByteArrayOutputStream()
            val buffer = ByteArray(16 * 1024)
            while (true) {
                val read = input.read(buffer)
                if (read < 0) break
                if (out.size() + read > limit) throw GatewayTransportException("response exceeds budget")
                out.write(buffer, 0, read)
            }
            return out.toString(Charsets.UTF_8.name())
        }
    }
}
