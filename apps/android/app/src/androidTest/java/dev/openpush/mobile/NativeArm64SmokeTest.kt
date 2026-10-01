package dev.openpush.mobile

import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import uniffi.openpush_mobile_bindings.MobileBindingsException
import uniffi.openpush_mobile_bindings.NativeIncomingSms
import uniffi.openpush_mobile_bindings.NativeOpenConfig
import uniffi.openpush_mobile_bindings.createSmokeVaultMaterial
import uniffi.openpush_mobile_bindings.openNativeClient
import java.util.UUID

@RunWith(AndroidJUnit4::class)
class NativeArm64SmokeTest {
    @Test
    fun arm64JniSqlCipherCryptoCaptureReopenAndTypedErrors() {
        assertEquals("arm64-v8a", android.os.Build.SUPPORTED_ABIS.first())
        val context = ApplicationProvider.getApplicationContext<android.content.Context>()
        val passphrase = "android emulator synthetic smoke phrase"
        val vaultId = UUID.randomUUID().toString()
        val deviceId = UUID.randomUUID().toString()
        val database = context.noBackupFilesDir.resolve("native-arm64-${UUID.randomUUID()}.sqlcipher")
        val material = createSmokeVaultMaterial(vaultId, passphrase)
        val config = NativeOpenConfig(database.absolutePath, vaultId, deviceId, ByteArray(32) { 0x35 })

        val wrongPassphrase = openNativeClient(config)
        try {
            wrongPassphrase.unlock(material.profileJson, material.headerJson, "deliberately wrong synthetic phrase")
            throw AssertionError("wrong passphrase unexpectedly unlocked synthetic vault")
        } catch (_: MobileBindingsException.WrongPassphrase) {
            // The generated binding preserved the Rust typed error across ARM64 JNI/JNA.
        } finally {
            wrongPassphrase.dispose()
        }

        val client = openNativeClient(config)
        client.unlock(material.profileJson, material.headerJson, passphrase)
        val captured = client.captureIncoming(
            NativeIncomingSms(null, "+15550135000", "ARM64 emulator SQLCipher crypto smoke", "arm64-smoke-1", false)
        )
        assertFalse(captured.duplicate)
        assertEquals(1, client.pendingOutboxJson().size)
        client.dispose()

        val reopened = openNativeClient(config)
        reopened.unlock(material.profileJson, material.headerJson, passphrase)
        val messages = reopened.messages(captured.conversationId)
        assertEquals(1, messages.size)
        assertEquals("ARM64 emulator SQLCipher crypto smoke", messages.single().body)
        assertTrue(reopened.markSeen(captured.messageId))
        reopened.dispose()

        try {
            reopened.listConversations()
            throw AssertionError("closed native client unexpectedly accepted a read")
        } catch (_: MobileBindingsException.Closed) {
            // Typed use-after-close error is part of the generated API contract.
        } finally {
            listOf("", "-wal", "-shm", "-journal").forEach { database.resolveSibling(database.name + it).delete() }
        }
    }
}
