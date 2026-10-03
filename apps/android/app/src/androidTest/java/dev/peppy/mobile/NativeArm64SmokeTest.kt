package dev.peppy.mobile

import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import uniffi.peppy_mobile_bindings.MobileBindingsException
import uniffi.peppy_mobile_bindings.NativeIncomingSms
import uniffi.peppy_mobile_bindings.NativeMmsAcquisitionInput
import uniffi.peppy_mobile_bindings.NativeMmsSource
import uniffi.peppy_mobile_bindings.NativeOpenConfig
import uniffi.peppy_mobile_bindings.NativeNotificationCapture
import uniffi.peppy_mobile_bindings.NativeNotificationCaptureOutcome
import uniffi.peppy_mobile_bindings.createSmokeVaultMaterial
import uniffi.peppy_mobile_bindings.openNativeClient
import java.util.UUID

@RunWith(AndroidJUnit4::class)
class NativeArm64SmokeTest {
    @Test
    fun mmsAcquisitionAndOwnAddressSurviveRealArm64Reopen() {
        val context = ApplicationProvider.getApplicationContext<android.content.Context>()
        val phrase = "synthetic MMS acquisition binding smoke phrase"
        val vault = UUID.randomUUID().toString()
        val device = UUID.randomUUID().toString()
        val database = context.noBackupFilesDir.resolve("mms-smoke-${UUID.randomUUID()}.sqlcipher")
        val config = NativeOpenConfig(database.absolutePath, vault, device, ByteArray(32) { 0x37 })
        val material = createSmokeVaultMaterial(vault, phrase)
        val own = "+15550135000"
        val sender = "+15550135001"
        val peer = "+15550135002"
        var client = openNativeClient(config)
        try {
            client.unlock(material.profileJson, material.headerJson, phrase)
            val input = NativeMmsAcquisitionInput(
                NativeMmsSource("synthetic-install", "synthetic-sim", "42", "7"),
                true, sender, listOf(own, peer), "Group subject", "Text-only group MMS", false, 42L, null,
            )
            val acquisition = client.beginMmsAcquisition(input)
            assertTrue(client.pendingOutboxJson().isEmpty())
            assertEquals(acquisition.acquisitionId, client.beginMmsAcquisition(input).acquisitionId)
            val captured = client.completeMmsAcquisition(acquisition.acquisitionId)
            assertFalse(captured.duplicate)
            assertTrue(client.mmsReplyContext(captured.conversationId).blockedReason != null)
            client.setMmsOwnAddress("synthetic-sim", own)
            val reply = client.mmsReplyContext(captured.conversationId)
            assertEquals(null, reply.blockedReason)
            assertEquals(setOf(sender, peer), reply.recipients.toSet())
            assertEquals("mms", client.messages(captured.conversationId).single().transport)
            client.dispose()
            client = openNativeClient(config)
            client.unlock(material.profileJson, material.headerJson, phrase)
            assertTrue(client.completeMmsAcquisition(acquisition.acquisitionId).duplicate)
            assertEquals(1, client.messages(captured.conversationId).size)
            assertEquals(setOf(sender, peer), client.mmsReplyContext(captured.conversationId).recipients.toSet())
        } finally {
            client.dispose()
            listOf("", "-wal", "-shm", "-journal").forEach { database.resolveSibling(database.name + it).delete() }
        }
    }

    @Test
    fun notificationBindingsCaptureFilterAndDismissWithRealArm64Store() {
        val context = ApplicationProvider.getApplicationContext<android.content.Context>()
        val phrase = "synthetic notification binding smoke phrase"
        val vault = UUID.randomUUID().toString()
        val source = UUID.randomUUID().toString()
        val database = context.noBackupFilesDir.resolve("notification-smoke-${UUID.randomUUID()}.sqlcipher")
        val material = createSmokeVaultMaterial(vault, phrase)
        val client = openNativeClient(NativeOpenConfig(database.absolutePath, vault, source, ByteArray(32) { 0x36 }))
        try {
            val input = NativeNotificationCapture("smoke-key", "first", "example.smoke", "Smoke", "Title", "Body", null, 42L, true)
            assertEquals(NativeNotificationCaptureOutcome.DROPPED_LOCKED, client.captureNotification(input))
            client.unlock(material.profileJson, material.headerJson, phrase)
            assertEquals(NativeNotificationCaptureOutcome.CAPTURED, client.captureNotification(input))
            val notification = client.notificationSnapshot().notifications.single()
            assertEquals(source, notification.target.sourceDeviceId)
            assertEquals("Body", notification.text)
            client.dismissNotification(notification.target)
            assertTrue(client.notificationSnapshot().notifications.single().dismissalPending)
            val effect = client.pendingNotificationDismissals(10uL).single()
            assertEquals("first", effect.instance)
            client.completeNotificationDismissal(effect.id)
            client.removeNotification("smoke-key", "latest-os-instance")
            assertTrue(client.notificationSnapshot().notifications.isEmpty())
            client.setAppMuted(source, "example.smoke", "Smoke", true)
            assertEquals(NativeNotificationCaptureOutcome.FILTERED_OUT, client.captureNotification(input))
        } finally {
            client.dispose()
            listOf("", "-wal", "-shm", "-journal").forEach { database.resolveSibling(database.name + it).delete() }
        }
    }

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
