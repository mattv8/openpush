package dev.peppy.mobile

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.util.UUID

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class CredentialParserTest {
    private val vault = UUID.randomUUID().toString()
    private val device = UUID.randomUUID().toString()

    private fun parse(json: String, debug: Boolean = false) = CredentialParser.parse(json.toByteArray(), allowDebugLoopback = debug)

    @Test
    fun acceptsStrictV1AndCanonicalizesIdentity() {
        val parsed = parse(credentialJson("https://Vault.Example/", vault.uppercase(), device))
        assertNotNull(parsed)
        parsed!!
        assertEquals("https://vault.example", parsed.origin)
        assertEquals(vault, parsed.vaultId)
        assertEquals(device, parsed.deviceId)
        assertEquals(false, parsed.toString().contains(TEST_TOKEN))
    }

    @Test
    fun rejectsMalformedOrUnsafeCredentials() {
        val rejected = listOf(
            credentialJson(vaultId = vault, deviceId = device, extra = ""","extra":"x""""),
            credentialJson(vaultId = vault, deviceId = device).replace("\"version\":1", "\"version\":2"),
            credentialJson(vaultId = vault, deviceId = device).replace("\"version\":1", "\"version\":\"1\""),
            credentialJson(vaultId = vault, deviceId = device, token = "ab".repeat(47)),
            credentialJson(vaultId = vault, deviceId = device, token = "zz".repeat(48)),
            credentialJson(origin = "http://vault.example", vaultId = vault, deviceId = device),
            credentialJson(origin = "https://vault.example/path", vaultId = vault, deviceId = device),
            credentialJson(origin = "https://user@vault.example", vaultId = vault, deviceId = device),
            credentialJson(origin = "https://vault.example?q=1", vaultId = vault, deviceId = device),
            credentialJson(vaultId = "1-1-1-1-1", deviceId = device),
            credentialJson(vaultId = vault, deviceId = "not-a-uuid"),
            "not json",
            credentialJson(vaultId = vault, deviceId = device) + " ".repeat(CredentialParser.MAX_BYTES),
        )
        rejected.forEach { assertNull(it.take(120), parse(it)) }
    }

    @Test
    fun cleartextOnlyForDebugLoopback() {
        assertNull(parse(credentialJson(origin = "http://10.0.2.2:8080", vaultId = vault, deviceId = device), debug = false))
        assertEquals("http://10.0.2.2:8080", parse(credentialJson(origin = "http://10.0.2.2:8080", vaultId = vault, deviceId = device), debug = true)?.origin)
        assertNull(parse(credentialJson(origin = "http://192.168.1.2", vaultId = vault, deviceId = device), debug = true))
    }
}
