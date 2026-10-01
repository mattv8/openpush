package dev.openpush.mobile

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.security.GeneralSecurityException
import java.security.KeyStore
import java.util.Base64
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Authenticated encryption for small secrets kept in app-private preferences. Every value is bound
 * to its [purpose] as AES-GCM associated data, so a wrapped bearer token can never be replayed as
 * the database key, key cache, or vault material (and vice versa).
 */
interface SecretBox {
    fun seal(purpose: String, plaintext: ByteArray): String

    /** Returns null for missing keys, tampering, purpose mismatch, or malformed input. */
    fun open(purpose: String, sealed: String): ByteArray?
}

/** AES-256-GCM with a 96-bit random IV and 128-bit tag. [key] is asked to create only on seal. */
class AesGcmSecretBox(private val key: (create: Boolean) -> SecretKey?) : SecretBox {
    override fun seal(purpose: String, plaintext: ByteArray): String {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, checkNotNull(key(true)) { "wrapping key unavailable" })
        cipher.updateAAD(aad(purpose))
        val iv = cipher.iv
        check(iv.size == IV_BYTES)
        return Base64.getEncoder().encodeToString(iv + cipher.doFinal(plaintext))
    }

    override fun open(purpose: String, sealed: String): ByteArray? = try {
        val bytes = Base64.getDecoder().decode(sealed)
        if (bytes.size < IV_BYTES + TAG_BYTES) {
            null
        } else {
            val secret = key(false)
            if (secret == null) {
                null
            } else {
                val cipher = Cipher.getInstance(TRANSFORMATION)
                cipher.init(Cipher.DECRYPT_MODE, secret, GCMParameterSpec(TAG_BYTES * 8, bytes, 0, IV_BYTES))
                cipher.updateAAD(aad(purpose))
                cipher.doFinal(bytes, IV_BYTES, bytes.size - IV_BYTES)
            }
        }
    } catch (_: GeneralSecurityException) {
        null
    } catch (_: IllegalArgumentException) {
        null
    }

    private fun aad(purpose: String) = "openpush-secret-box-v1\u0000$purpose".toByteArray(Charsets.UTF_8)

    private companion object {
        const val TRANSFORMATION = "AES/GCM/NoPadding"
        const val IV_BYTES = 12
        const val TAG_BYTES = 16
    }
}

/** Non-exportable Android Keystore key. A missing key is only generated when sealing new data. */
object KeystoreSecretBox : SecretBox by AesGcmSecretBox({ create -> keystoreKey(create) })

private const val KEYSTORE_ALIAS = "openpush.secret-box.v1"

private fun keystoreKey(create: Boolean): SecretKey? {
    val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
    (store.getKey(KEYSTORE_ALIAS, null) as? SecretKey)?.let { return it }
    if (!create) return null
    return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").apply {
        init(
            KeyGenParameterSpec.Builder(KEYSTORE_ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setKeySize(256)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .build(),
        )
    }.generateKey()
}
