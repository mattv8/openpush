package dev.openpush.mobile.smoke

import java.io.File
import java.util.UUID
import uniffi.openpush_mobile_bindings.MobileBindingsException
import uniffi.openpush_mobile_bindings.NativeIncomingSms
import uniffi.openpush_mobile_bindings.NativeOpenConfig
import uniffi.openpush_mobile_bindings.createSmokeVaultMaterial
import uniffi.openpush_mobile_bindings.openNativeClient

fun main() {
    val passphrase = "mobile smoke passphrase"
    val vaultId = UUID.randomUUID().toString()
    val deviceId = UUID.randomUUID().toString()
    val database = File(System.getProperty("java.io.tmpdir"), "openpush-native-smoke-${UUID.randomUUID()}.db")
    val material = createSmokeVaultMaterial(vaultId, passphrase)
    val config = NativeOpenConfig(
        database.absolutePath,
        vaultId,
        deviceId,
        ByteArray(32) { 7 },
    )

    val client = openNativeClient(config)
    client.unlock(material.profileJson, material.headerJson, passphrase)
    val captured = client.captureIncoming(
        NativeIncomingSms(
            null,
            "+15551234567",
            "SQLCipher-backed generated Kotlin smoke",
            "kotlin-smoke-1",
            false,
        )
    )
    check(!captured.duplicate)
    check(client.pendingOutboxJson().size == 1)
    client.dispose()

    val reopened = openNativeClient(config)
    reopened.unlock(material.profileJson, material.headerJson, passphrase)
    val messages = reopened.messages(captured.conversationId)
    check(messages.size == 1)
    check(messages.single().body == "SQLCipher-backed generated Kotlin smoke")
    check(reopened.markSeen(captured.messageId))
    reopened.dispose()
    try {
        reopened.listConversations()
        error("closed NativeClient unexpectedly accepted a read")
    } catch (_: MobileBindingsException.Closed) {
        println("Kotlin SQLCipher UniFFI smoke passed")
    }
}
