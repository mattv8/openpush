package dev.peppy.mobile

import android.content.Context
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import uniffi.peppy_mobile_bindings.NativeIncomingSms
import java.util.UUID

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class SecureImportTest : GatewayTestBase() {
    private fun secure() = context.getSharedPreferences("peppy-secure", Context.MODE_PRIVATE)

    @Test
    fun databaseOpenIsDistinctFromSharedKeyUnlock() {
        assertEquals(ImportResult.IMPORTED, enroll())
        val locked = NativeGateway.status(context)
        assertTrue(locked.enrolled && locked.databaseOpen)
        assertFalse(locked.sharedKeysReady)
        assertNull("no background session while the shared vault is locked", NativeGateway.session(context))

        assertEquals(UnlockResult.WRONG_PASSPHRASE, NativeGateway.unlock(context, "a new per-device passphrase"))
        assertFalse(NativeGateway.status(context).sharedKeysReady)
        assertEquals(UnlockResult.UNLOCKED, NativeGateway.unlock(context, TEST_PASSPHRASE))
        assertTrue(NativeGateway.status(context).sharedKeysReady)
        assertEquals(TEST_TOKEN, NativeGateway.session(context)?.bearerToken)
    }

    @Test
    fun keystoreWrappedKeyCacheRestoresKeysAfterProcessDeathWithoutPassphrase() {
        enrollAndUnlock()
        NativeGateway.closeForTest() // process death
        assertTrue(NativeGateway.status(context).sharedKeysReady)

        // Purpose binding: a cache sealed as another purpose (here, the token slot) is rejected.
        val prefs = secure()
        prefs.edit().putString("key-cache.v1", prefs.getString("token.v1", null)).commit()
        NativeGateway.closeForTest()
        val status = NativeGateway.status(context)
        assertTrue(status.databaseOpen)
        assertFalse(status.sharedKeysReady)
    }

    @Test
    fun secretsAreNeverStoredInPlaintext() {
        enrollAndUnlock()
        val stored = secure().all.values.joinToString("\n")
        assertFalse(stored.contains(TEST_TOKEN))
        assertFalse(stored.contains(material.headerJson))
        listOf("db-key.v1", "token.v1", "profile.v1", "header.v1", "key-cache.v1").forEach { assertNotNull(it, secure().getString(it, null)) }
    }

    @Test
    fun reimportOfSameDevicePreservesDatabaseKeyAndData() {
        val client = enrollAndUnlock()
        val captured = client.captureIncoming(NativeIncomingSms(null, "+15550001111", "kept across reimport", "provider-1", false))
        val keyBefore = secure().getString("db-key.v1", null)
        NativeGateway.closeForTest()

        val rotatedToken = "cd".repeat(48)
        assertEquals(ImportResult.UPDATED, NativeGateway.persistVerified(context, parsed(token = rotatedToken), vaultMaterial()))
        assertEquals(keyBefore, secure().getString("db-key.v1", null))
        val reopened = checkNotNull(NativeGateway.open(context))
        assertEquals("kept across reimport", reopened.messages(captured.conversationId).single().body)
        assertTrue("unchanged vault material keeps the key cache", NativeGateway.status(context).sharedKeysReady)
        assertEquals(rotatedToken, NativeGateway.session(context)?.bearerToken)
    }

    @Test
    fun differentBindingIsRefusedWithoutTouchingStoredSecrets() {
        enrollAndUnlock()
        val before = secure().all.toMap()
        listOf(
            parsed(vault = UUID.randomUUID().toString()),
            parsed(device = UUID.randomUUID().toString()),
            parsed(origin = "https://other.example"),
        ).forEach { other ->
            assertEquals(ImportResult.IDENTITY_MISMATCH, NativeGateway.persistVerified(context, other, vaultMaterial()))
        }
        assertEquals(before, secure().all.toMap())
        assertTrue(NativeGateway.status(context).sharedKeysReady)
    }

    @Test
    fun provisionNeverCreatesAKeyOverExistingData() {
        enrollAndUnlock()
        NativeGateway.closeForTest()
        // Lose the binding (e.g. preferences cleared) while the encrypted database remains.
        secure().edit().clear().commit()
        val database = NativeGateway.databaseFile(context)
        val sizeBefore = database.length()
        assertTrue(database.exists())

        assertEquals(ImportResult.EXISTING_DATA_WITHOUT_KEY, enroll())
        assertNull(secure().getString("db-key.v1", null))
        assertEquals(sizeBefore, database.length())
        assertNull(NativeGateway.open(context))
    }

    @Test
    fun lostKeystoreKeyFailsClosedInsteadOfRegenerating() {
        enrollAndUnlock()
        NativeGateway.closeForTest()
        keys.key = null // Keystore entry lost; wrapped values can no longer be opened.
        assertNull(NativeGateway.open(context))
        assertEquals(UnlockResult.DATABASE_UNAVAILABLE, NativeGateway.unlock(context, TEST_PASSPHRASE))
        val before = secure().all.toMap()
        assertEquals(ImportResult.EXISTING_DATA_WITHOUT_KEY, enroll())
        assertEquals(before, secure().all.toMap())
    }

    @Test
    fun serverVaultResponseMustMatchCredentialRoleAndStrictEpoch() {
        val credential = parsed()
        val header = java.util.Base64.getEncoder().encodeToString(material.headerJson.toByteArray())
        val epoch = org.json.JSONObject(material.profileJson).getInt("key_epoch")
        fun body(vault: String = vaultId, device: String = deviceId, role: String = "gateway", keyEpoch: String = epoch.toString()) =
            """{"vault_id":"$vault","device_id":"$device","role":"$role","key_epoch":$keyEpoch,"profile_fingerprint":"x",""" +
                """"public_key_profile":${material.profileJson},"encrypted_vault_check_header":"$header"}"""
        assertTrue(NativeGateway.verifyVault(credential, body()) is NativeGateway.VaultCheck.Verified)
        assertEquals(NativeGateway.VaultCheck.NotGateway, NativeGateway.verifyVault(credential, body(role = "device")))
        assertEquals(NativeGateway.VaultCheck.Mismatch, NativeGateway.verifyVault(credential, body(vault = UUID.randomUUID().toString())))
        assertEquals(NativeGateway.VaultCheck.Mismatch, NativeGateway.verifyVault(credential, body(device = UUID.randomUUID().toString())))
        listOf("\"$epoch\"", "${epoch + 1}", "$epoch.5", "-1", "4294967296").forEach { bad ->
            assertEquals(bad, NativeGateway.VaultCheck.Mismatch, NativeGateway.verifyVault(credential, body(keyEpoch = bad)))
        }
        assertEquals(NativeGateway.VaultCheck.Mismatch, NativeGateway.verifyVault(credential, "{}"))
    }

    @Test
    fun freshImportStartsInBootstrapAndReimportKeepsPhase() {
        assertEquals(ImportResult.IMPORTED, enroll())
        assertEquals(SyncPhase.BOOTSTRAP, GatewayStateStore(context).phase)
        GatewayStateStore(context).advance(SyncPhase.LIVE)
        assertEquals(ImportResult.UPDATED, enroll())
        assertEquals(SyncPhase.LIVE, GatewayStateStore(context).phase)
        GatewayStateStore(context).requireRecovery(RecoveryReason.PRODUCER_CONFLICT)
        GatewayStateStore(context).advance(SyncPhase.LIVE)
        assertEquals("recovery is only left by a new enrollment", SyncPhase.RECOVERY_REQUIRED, GatewayStateStore(context).phase)
    }

    @Test
    fun concurrentReimportWithNewVaultMaterialNeverLeavesAStaleKeyCache() {
        assertEquals(ImportResult.IMPORTED, enroll())
        val rotated = uniffi.peppy_mobile_bindings.createSmokeVaultMaterial(vaultId, TEST_PASSPHRASE)
        var result: UnlockResult? = null
        val unlocking = Thread { result = NativeGateway.unlock(context, TEST_PASSPHRASE) }.apply { start() }
        Thread.sleep(50)
        assertEquals(ImportResult.UPDATED, NativeGateway.persistVerified(context, parsed(), VaultMaterial(rotated.profileJson, rotated.headerJson)))
        unlocking.join(120_000)
        assertNotNull(result)
        val status = NativeGateway.status(context)
        assertFalse("keys from the replaced vault material must not count as ready", status.sharedKeysReady)
        assertNull(secure().getString("key-cache.v1", null))
    }

    @Test
    fun lostInitializedDatabaseIsNeverRecreatedAndEntersRecovery() {
        val client = enrollAndUnlock()
        GatewayStateStore(context).advance(SyncPhase.LIVE)
        val command = queueCommand(client, SimRoutes.routeId(3), "must never run after file loss")
        assertTrue(client.pendingCommands().any { it.commandId == command })
        NativeGateway.closeForTest()
        val database = NativeGateway.databaseFile(context)
        val before = secure().all.toMap()
        assertTrue("remove only the test database file", database.delete())

        assertNull(NativeGateway.open(context))
        assertFalse("no new database was created", database.exists())
        val status = NativeGateway.status(context)
        assertFalse(status.databaseOpen || status.sharedKeysReady)
        assertEquals(SyncPhase.RECOVERY_REQUIRED, status.syncPhase)
        assertEquals(RecoveryReason.DATABASE_MISSING, status.recoveryReason)
        assertNull("no session, so no sync, permit, or carrier work", NativeGateway.session(context))
        assertEquals(UnlockResult.DATABASE_UNAVAILABLE, NativeGateway.unlock(context, TEST_PASSPHRASE))
        assertEquals("keys and markers preserved", before, secure().all.toMap())

        // Re-importing the same credential neither clears the guard nor creates a database.
        assertEquals(ImportResult.UPDATED, enroll())
        assertNull(NativeGateway.open(context))
        assertFalse(database.exists())
        assertEquals(SyncPhase.RECOVERY_REQUIRED, GatewayStateStore(context).phase)
        assertEquals(RecoveryReason.DATABASE_MISSING, GatewayStateStore(context).recoveryReason)
    }

    @Test
    fun missingDatabaseOutsideFreshBootstrapIsNeverCreatedEvenBeforeFirstOpen() {
        assertEquals(ImportResult.IMPORTED, enroll())
        GatewayStateStore(context).advance(SyncPhase.BOOTSTRAP_DRAINING)
        assertNull(NativeGateway.open(context))
        assertFalse(NativeGateway.databaseFile(context).exists())
        assertEquals(RecoveryReason.DATABASE_MISSING, GatewayStateStore(context).recoveryReason)
    }

    @Test
    fun archiveAfterRevokePreservesOldCiphertextAndAllowsNewEnrollment() {
        assertEquals(ImportResult.IMPORTED, enroll())
        assertNotNull(NativeGateway.open(context))
        val database = NativeGateway.databaseFile(context)
        assertTrue(database.exists())

        assertTrue(NativeGateway.archiveEnrollment(context))
        assertFalse(database.exists())
        assertNull(secure().getString("identity.v1", null))
        assertEquals(SyncPhase.BOOTSTRAP, GatewayStateStore(context).phase)
        val archived = context.noBackupFilesDir.resolve("peppy-archives").listFiles().orEmpty().single()
        assertTrue(archived.resolve("peppy.sqlcipher").exists())
        assertTrue(archived.resolve("db-key.sealed").exists())

        assertEquals(ImportResult.IMPORTED, NativeGateway.persistVerified(context, parsed(device = UUID.randomUUID().toString()), vaultMaterial()))
    }
}
