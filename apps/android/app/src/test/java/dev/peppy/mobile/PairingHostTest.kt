package dev.peppy.mobile

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class PairingHostTest {
    @Test
    fun parsesOnlyCanonicalQrPayload() {
        val token = "a".repeat(43)
        val parsed = PairingHost.parseQr("""{"https_origin":"https://Vault.Example/","intent_token":"$token"}""")

        assertEquals("https://vault.example", parsed?.origin)
        assertEquals(token, parsed?.intentToken)
    }

    @Test
    fun rejectsQrPayloadsWithSecretsOrInvalidOrigins() {
        val token = "a".repeat(43)
        assertNull(PairingHost.parseQr("""{"https_origin":"https://vault.example","intent_token":"$token","owner_token":"secret"}"""))
        assertNull(PairingHost.parseQr("""{"https_origin":"http://vault.example","intent_token":"$token"}"""))
        assertNull(PairingHost.parseQr("""{"https_origin":"https://vault.example/path","intent_token":"$token"}"""))
        assertNull(PairingHost.parseQr("""{"https_origin":"https://vault.example?redirect=evil","intent_token":"$token"}"""))
        assertNull(PairingHost.parseQr("""{"https_origin":"https://user@vault.example","intent_token":"$token"}"""))
        assertNull(PairingHost.parseQr("""{"https_origin":"https://vault.example","intent_token":"short"}"""))
        assertNull(PairingHost.parseQr("""{"https_origin":"https://vault.example","intent_token":"${"a".repeat(44)}"}"""))
    }
}
