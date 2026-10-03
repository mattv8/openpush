package dev.peppy.mobile

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import org.json.JSONObject
import org.junit.After
import org.junit.Before
import org.robolectric.Shadows.shadowOf
import uniffi.peppy_mobile_bindings.NativeClient
import uniffi.peppy_mobile_bindings.NativeComposeDraftUpdate
import uniffi.peppy_mobile_bindings.NativeOpenConfig
import uniffi.peppy_mobile_bindings.NativeVaultMaterial
import uniffi.peppy_mobile_bindings.createSmokeVaultMaterial
import uniffi.peppy_mobile_bindings.openNativeClient
import java.io.File
import java.nio.file.Files
import java.util.UUID
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey

const val TEST_PASSPHRASE = "robolectric shared vault passphrase"
const val TEST_ORIGIN = "https://vault.example"
val TEST_TOKEN = "ab".repeat(48)

fun credentialJson(
    origin: String = TEST_ORIGIN,
    vaultId: String,
    deviceId: String,
    token: String = TEST_TOKEN,
    extra: String = "",
) = """{"version":1,"origin":"$origin","vaultId":"$vaultId","deviceId":"$deviceId","deviceToken":"$token"$extra}"""

/** Software stand-in for the Android Keystore key (Robolectric has no AndroidKeyStore provider). */
class TestKeyHolder {
    var key: SecretKey? = null
    val box = AesGcmSecretBox { create -> key ?: if (create) KeyGenerator.getInstance("AES").apply { init(256) }.generateKey().also { key = it } else null }
}

/**
 * One synthetic vault per test JVM sandbox. Argon2 in the debug host library is slow, so the
 * desktop-role client is opened and unlocked once and reused to author commands.
 */
object SharedVault {
    val vaultId: String = UUID.randomUUID().toString()
    val gatewayDeviceId: String = UUID.randomUUID().toString()
    val material: NativeVaultMaterial by lazy { createSmokeVaultMaterial(vaultId, TEST_PASSPHRASE) }
    val desktop: NativeClient by lazy {
        val dir = Files.createTempDirectory("peppy-desktop").toFile().apply { deleteOnExit() }
        openNativeClient(NativeOpenConfig(File(dir, "desktop.db").path, vaultId, UUID.randomUUID().toString(), ByteArray(32) { 5 }))
            .apply { unlock(material.profileJson, material.headerJson, TEST_PASSPHRASE) }
    }
}

/**
 * Real generated bindings + host SQLCipher library. Each test gets a fresh app sandbox and resets
 * the process-wide owner, simulating a cold process start.
 */
abstract class GatewayTestBase {
    lateinit var context: Context
    lateinit var vaultId: String
    lateinit var deviceId: String
    lateinit var material: NativeVaultMaterial
    val keys = TestKeyHolder()
    val scheduled = mutableListOf<String>()
    private var cursor = 0

    @Before
    fun resetOwner() {
        context = ApplicationProvider.getApplicationContext()
        NativeGateway.closeForTest()
        NativeGateway.secretBox = keys.box
        GatewayScheduler.schedule = { scheduled += "sync" }
        vaultId = SharedVault.vaultId
        deviceId = SharedVault.gatewayDeviceId
        material = SharedVault.material
    }

    @After
    fun closeOwner() {
        NativeGateway.closeForTest()
    }

    fun vaultMaterial() = VaultMaterial(material.profileJson, material.headerJson)

    fun parsed(origin: String = TEST_ORIGIN, vault: String = vaultId, device: String = deviceId, token: String = TEST_TOKEN) =
        checkNotNull(CredentialParser.parse(credentialJson(origin, vault, device, token).toByteArray(), allowDebugLoopback = false))

    fun enroll(): ImportResult = NativeGateway.persistVerified(context, parsed(), vaultMaterial())

    fun enrollAndUnlock(): NativeClient {
        check(enroll() == ImportResult.IMPORTED)
        check(NativeGateway.unlock(context, TEST_PASSPHRASE) == UnlockResult.UNLOCKED)
        return checkNotNull(NativeGateway.open(context))
    }

    fun grant(vararg permissions: String) = shadowOf(context as android.app.Application).grantPermissions(*permissions)

    fun deny(vararg permissions: String) = shadowOf(context as android.app.Application).denyPermissions(*permissions)

    /** A sealed command envelope from a second (desktop-role) core client of the same vault. */
    fun desktopCommandEnvelope(route: String, body: String, recipients: List<String> = listOf("+15551234567")): Pair<String, String> {
        val desktop = SharedVault.desktop
        val draft = desktop.createComposeDraft(null)
        val routeJson = JSONObject().put("gateway_device_id", deviceId).put("subscription_id", route).toString()
        val saved = desktop.saveComposeDraft(draft.draftId, draft.revision, NativeComposeDraftUpdate(body, recipients, emptyList(), routeJson))
        val queued = desktop.sendComposeDraft(saved.draftId, saved.revision)
        val wire = desktop.pendingOutboxJson().single { JSONObject(it).getString("envelope_id") == queued.envelopeId }
        desktop.ackOutbox(queued.envelopeId)
        return queued.commandId to wire
    }

    /** Journals and applies the desktop envelope on the gateway core exactly as HTTP replay would. */
    fun queueCommand(gateway: NativeClient, route: String, body: String, recipients: List<String> = listOf("+15551234567")): String {
        val (commandId, wire) = desktopCommandEnvelope(route, body, recipients)
        gateway.ingestRaw(wire.toByteArray(), (++cursor).toString())
        gateway.applyPending(50uL)
        return commandId
    }
}
