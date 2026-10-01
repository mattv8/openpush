package dev.openpush.mobile

import android.content.Context
import android.content.SharedPreferences
import androidx.annotation.VisibleForTesting
import org.json.JSONException
import org.json.JSONObject
import uniffi.openpush_mobile_bindings.MobileBindingsException
import uniffi.openpush_mobile_bindings.NativeClient
import uniffi.openpush_mobile_bindings.NativeOpenConfig
import uniffi.openpush_mobile_bindings.openNativeClient
import java.io.File
import java.io.IOException
import java.security.GeneralSecurityException
import java.security.SecureRandom
import java.util.Base64

enum class ImportResult {
    /** First enrollment: a new SQLCipher key was generated for this vault device. */
    IMPORTED,
    /** Same origin/vault/device re-imported: token and vault material refreshed, DB key kept. */
    UPDATED,
    INVALID_CREDENTIAL,
    SERVER_REJECTED,
    NOT_A_GATEWAY,
    NETWORK_ERROR,
    /** A different origin/vault/device is already bound to this install's database. */
    IDENTITY_MISMATCH,
    /** Local data exists without its wrapped key; a new key would orphan it, so import refuses. */
    EXISTING_DATA_WITHOUT_KEY,
    STORAGE_ERROR,
}

enum class UnlockResult {
    UNLOCKED,
    NOT_ENROLLED,
    DATABASE_UNAVAILABLE,
    WRONG_PASSPHRASE,
    /** A credential re-import replaced the binding while unlocking; nothing was cached. */
    SUPERSEDED,
    FAILED,
}

/** `databaseOpen` (SQLCipher key accepted) is deliberately distinct from shared-vault key readiness. */
data class GatewayStatus(
    val enrolled: Boolean,
    val origin: String?,
    val databaseOpen: Boolean,
    val sharedKeysReady: Boolean,
    val syncPhase: SyncPhase = SyncPhase.BOOTSTRAP,
    val recoveryReason: RecoveryReason? = null,
    val outboxRejection: String? = null,
)

/** Only produced when the database is open and the shared vault purpose keys are installed. */
class GatewaySession(val client: NativeClient, val origin: String, val deviceId: String, internal val bearerToken: String)

/**
 * The one process-wide owner of the generated [NativeClient]. Rust remains the sole owner of
 * message, outbox, journal and carrier-attempt state; this object only holds the binding identity
 * and Keystore-wrapped secrets:
 *  - the SQLCipher database key, generated exactly once per install binding and never replaced;
 *  - the device bearer token and the public vault profile/check header;
 *  - the opaque C3 purpose-key cache, so receivers and workers never run Argon2.
 */
object NativeGateway {
    private const val PREFS = "openpush-secure"
    private const val P_IDENTITY = "identity.v1"
    private const val P_DB_KEY = "db-key.v1"
    private const val P_TOKEN = "token.v1"
    private const val P_PROFILE = "profile.v1"
    private const val P_HEADER = "header.v1"
    private const val P_KEY_CACHE = "key-cache.v1"
    /** Set after the first successful open; a missing database file afterwards is never recreated. */
    private const val P_DB_INITIALIZED = "db-initialized.v1"
    private const val DATABASE_NAME = "openpush.sqlcipher"
    private const val DATABASE_KEY_BYTES = 32
    private const val MAX_VAULT_RESPONSE_BYTES = 256 * 1024

    @VisibleForTesting
    @Volatile
    internal var secretBox: SecretBox = KeystoreSecretBox

    private val lock = Any()
    /** Serializes passphrase unlocks; never held while the owner [lock] is needed by receivers. */
    private val unlockLock = Any()
    private var client: NativeClient? = null
    private var sharedKeysReady = false
    /** Incremented whenever the open client or its vault material is replaced. */
    private var generation = 0L

    private data class Identity(val origin: String, val vaultId: String, val deviceId: String) {
        fun encode() = "$origin\n$vaultId\n$deviceId"
    }

    fun databaseFile(context: Context) = File(context.noBackupFilesDir, DATABASE_NAME)

    /** Opens (once) the SQLCipher database. Does not imply the shared vault keys are available. */
    fun open(context: Context): NativeClient? = synchronized(lock) {
        client?.let { return it }
        generation++
        val prefs = prefs(context)
        val identity = identity(prefs) ?: return null
        // SQLCipher would silently create an empty database at a missing path. That is only
        // legitimate for the very first open of a fresh enrollment (no initialized marker, still in
        // BOOTSTRAP). Otherwise the file was lost: record recovery and never create a new one.
        val state = GatewayStateStore(context)
        if (!databaseFile(context).exists() &&
            (prefs.getBoolean(P_DB_INITIALIZED, false) || state.phase != SyncPhase.BOOTSTRAP)
        ) {
            try {
                if (state.phase != SyncPhase.RECOVERY_REQUIRED) state.requireRecovery(RecoveryReason.DATABASE_MISSING)
            } catch (_: IllegalStateException) {
                // Not persisted this time; the open is still refused and retried on next access.
            }
            return null
        }
        val key = prefs.getString(P_DB_KEY, null)?.let { secretBox.open(P_DB_KEY, it) } ?: return null
        val opened = try {
            if (key.size != DATABASE_KEY_BYTES) return null
            openNativeClient(NativeOpenConfig(databaseFile(context).absolutePath, identity.vaultId, identity.deviceId, key))
        } catch (_: MobileBindingsException) {
            return null
        } finally {
            key.fill(0)
        }
        if (!prefs.getBoolean(P_DB_INITIALIZED, false) && !prefs.edit().putBoolean(P_DB_INITIALIZED, true).commit()) {
            opened.dispose()
            return null
        }
        sharedKeysReady = importCachedKeys(prefs, opened)
        client = opened
        opened
    }

    fun status(context: Context): GatewayStatus {
        val opened = open(context) != null
        val state = GatewayStateStore(context)
        return synchronized(lock) {
            val identity = identity(prefs(context))
            GatewayStatus(
                identity != null, identity?.origin, opened, opened && sharedKeysReady,
                state.phase, state.recoveryReason, state.outboxRejection,
            )
        }
    }

    /** A ready session for background sync, or null while not enrolled, unopenable, or locked. */
    fun session(context: Context): GatewaySession? {
        val opened = open(context) ?: return null
        return synchronized(lock) {
            if (!sharedKeysReady) return null
            val prefs = prefs(context)
            val identity = identity(prefs) ?: return null
            val token = prefs.getString(P_TOKEN, null)?.let { secretBox.open(P_TOKEN, it) } ?: return null
            GatewaySession(opened, identity.origin, identity.deviceId, String(token, Charsets.UTF_8))
        }
    }

    /** Bounded file bytes → strict parse → authenticated `/v1/vault` binding → persistence. */
    fun importCredential(context: Context, credentialBytes: ByteArray): ImportResult {
        val credential = CredentialParser.parse(credentialBytes) ?: return ImportResult.INVALID_CREDENTIAL
        val response = try {
            GatewayHttp(credential.origin, credential.deviceToken).get("/v1/vault")
        } catch (_: IOException) {
            return ImportResult.NETWORK_ERROR
        }
        if (!response.ok) {
            return if (response.code == 401 || response.code == 403) ImportResult.SERVER_REJECTED else ImportResult.NETWORK_ERROR
        }
        val body = response.body?.takeIf { it.length <= MAX_VAULT_RESPONSE_BYTES } ?: return ImportResult.SERVER_REJECTED
        val material = when (val verified = verifyVault(credential, body)) {
            is VaultCheck.Verified -> verified.material
            VaultCheck.NotGateway -> return ImportResult.NOT_A_GATEWAY
            VaultCheck.Mismatch -> return ImportResult.SERVER_REJECTED
        }
        return persistVerified(context, credential, material)
    }

    internal sealed interface VaultCheck {
        class Verified(val material: VaultMaterial) : VaultCheck
        data object NotGateway : VaultCheck
        data object Mismatch : VaultCheck
    }

    /** The server must confirm the credential's own vault/device identity and gateway role. */
    internal fun verifyVault(credential: DeviceCredential, body: String): VaultCheck = try {
        val vault = JSONObject(body)
        val profile = vault.getJSONObject("public_key_profile")
        val epoch = vault.strictUInt("key_epoch")
        when {
            vault.getString("vault_id") != credential.vaultId ||
                vault.getString("device_id") != credential.deviceId ||
                profile.getString("vault_id") != credential.vaultId -> VaultCheck.Mismatch
            epoch == null || profile.strictUInt("key_epoch") != epoch -> VaultCheck.Mismatch
            vault.getString("role") != "gateway" -> VaultCheck.NotGateway
            else -> {
                val header = String(Base64.getMimeDecoder().decode(vault.getString("encrypted_vault_check_header")), Charsets.UTF_8)
                JSONObject(header) // must itself be JSON; Rust parses it strictly at unlock.
                VaultCheck.Verified(VaultMaterial(profile.toString(), header))
            }
        }
    } catch (_: JSONException) {
        VaultCheck.Mismatch
    } catch (_: IllegalArgumentException) {
        VaultCheck.Mismatch
    }

    /**
     * Persists an authenticated credential. The binding (origin, vault, device) is immutable: a
     * different binding is refused, and a re-import of the same binding never touches the
     * database key. A first import refuses if any database file or key record already exists.
     */
    internal fun persistVerified(context: Context, credential: DeviceCredential, material: VaultMaterial): ImportResult =
        synchronized(lock) {
            val prefs = prefs(context)
            val requested = Identity(credential.origin, credential.vaultId, credential.deviceId)
            val existing = identity(prefs)
            val editor = prefs.edit()
            val result = try {
                if (existing != null) {
                    if (existing != requested) return ImportResult.IDENTITY_MISMATCH
                    // The wrapped key must still open (Keystore entry intact); never replace it.
                    val key = prefs.getString(P_DB_KEY, null)?.let { secretBox.open(P_DB_KEY, it) }
                        ?: return ImportResult.EXISTING_DATA_WITHOUT_KEY
                    key.fill(0)
                    val profileChanged = openString(prefs, P_PROFILE) != material.profileJson ||
                        openString(prefs, P_HEADER) != material.headerJson
                    if (profileChanged) {
                        // New vault material may mean a new epoch: require a fresh passphrase unlock.
                        editor.remove(P_KEY_CACHE)
                        client?.dispose()
                        client = null
                        sharedKeysReady = false
                        generation++
                    }
                    ImportResult.UPDATED
                } else {
                    if (prefs.contains(P_DB_KEY) || prefs.contains(P_KEY_CACHE) || hasDatabaseFiles(context)) {
                        return ImportResult.EXISTING_DATA_WITHOUT_KEY
                    }
                    check(client == null)
                    val key = ByteArray(DATABASE_KEY_BYTES).also(SecureRandom()::nextBytes)
                    try {
                        editor.putString(P_DB_KEY, secretBox.seal(P_DB_KEY, key))
                    } finally {
                        key.fill(0)
                    }
                    editor.putString(P_IDENTITY, requested.encode())
                    ImportResult.IMPORTED
                }
            } catch (_: GeneralSecurityException) {
                return ImportResult.STORAGE_ERROR
            } catch (_: IllegalStateException) {
                return ImportResult.STORAGE_ERROR
            }
            try {
                editor.putString(P_TOKEN, secretBox.seal(P_TOKEN, credential.deviceToken.toByteArray(Charsets.UTF_8)))
                    .putString(P_PROFILE, secretBox.seal(P_PROFILE, material.profileJson.toByteArray(Charsets.UTF_8)))
                    .putString(P_HEADER, secretBox.seal(P_HEADER, material.headerJson.toByteArray(Charsets.UTF_8)))
            } catch (_: GeneralSecurityException) {
                return ImportResult.STORAGE_ERROR
            }
            if (!editor.commit()) return ImportResult.STORAGE_ERROR
            // A new database always bootstraps from a snapshot before any live replay or effect.
            if (result == ImportResult.IMPORTED) GatewayStateStore(context).startBootstrap()
            result
        }

    /**
     * Unlocks with the EXISTING shared vault passphrase (the one set when the vault was created),
     * then caches the opaque purpose keys under Keystore so background work never needs it again.
     */
    fun unlock(context: Context, passphrase: String): UnlockResult = synchronized(unlockLock) {
        val prefs = prefs(context)
        if (identity(prefs) == null) return UnlockResult.NOT_ENROLLED
        val opened = open(context) ?: return UnlockResult.DATABASE_UNAVAILABLE
        val (startGeneration, profile, header) = synchronized(lock) {
            Triple(
                generation,
                openString(prefs, P_PROFILE) ?: return UnlockResult.NOT_ENROLLED,
                openString(prefs, P_HEADER) ?: return UnlockResult.NOT_ENROLLED,
            )
        }
        val epoch = try {
            JSONObject(profile).strictUInt("key_epoch") ?: return UnlockResult.FAILED
        } catch (_: JSONException) {
            return UnlockResult.FAILED
        }
        return try {
            // Argon2 runs outside the owner lock so receivers are never blocked behind it.
            opened.unlock(profile, header, passphrase)
            // Only a manual passphrase+header unlock may activate the validated profile epoch
            // (idempotent when already active). Credential import and cached-key restore never do,
            // so a manual key rotation stays an explicit user action. Without this, keys for a
            // rotated profile install while the core's active epoch stays older and every command
            // is refused as STALE/EPOCH_NOT_ACTIVE.
            opened.activateVerifiedEpoch(epoch)
            val cache = opened.exportNativeKeyCacheForNativeStorage(epoch)
            val sealed = try { secretBox.seal(P_KEY_CACHE, cache) } finally { cache.fill(0) }
            synchronized(lock) {
                // A concurrent re-import may have replaced the client or vault material meanwhile.
                if (generation != startGeneration || client !== opened || openString(prefs, P_PROFILE) != profile) {
                    return UnlockResult.SUPERSEDED
                }
                if (!prefs.edit().putString(P_KEY_CACHE, sealed).commit()) return UnlockResult.FAILED
                sharedKeysReady = true
            }
            UnlockResult.UNLOCKED
        } catch (_: MobileBindingsException.WrongPassphrase) {
            UnlockResult.WRONG_PASSPHRASE
        } catch (_: MobileBindingsException) {
            UnlockResult.FAILED
        } catch (_: GeneralSecurityException) {
            UnlockResult.FAILED
        }
    }

    /** Simulates process death for tests: drops the in-memory owner without touching storage. */
    @VisibleForTesting
    internal fun closeForTest() = synchronized(lock) {
        client?.dispose()
        client = null
        sharedKeysReady = false
        generation++
    }

    private fun importCachedKeys(prefs: SharedPreferences, opened: NativeClient): Boolean {
        val cache = prefs.getString(P_KEY_CACHE, null)?.let { secretBox.open(P_KEY_CACHE, it) } ?: return false
        return try {
            opened.importNativeKeyCacheFromNativeStorage(cache)
            true
        } catch (_: MobileBindingsException) {
            false
        } finally {
            cache.fill(0)
        }
    }

    private fun hasDatabaseFiles(context: Context): Boolean {
        val db = databaseFile(context)
        return listOf("", "-wal", "-shm", "-journal").any { File(db.path + it).exists() }
    }

    private fun openString(prefs: SharedPreferences, purpose: String): String? =
        prefs.getString(purpose, null)?.let { secretBox.open(purpose, it) }?.toString(Charsets.UTF_8)

    private fun identity(prefs: SharedPreferences): Identity? =
        prefs.getString(P_IDENTITY, null)?.split('\n')?.takeIf { it.size == 3 }?.let { Identity(it[0], it[1], it[2]) }

    private fun prefs(context: Context) = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
}
