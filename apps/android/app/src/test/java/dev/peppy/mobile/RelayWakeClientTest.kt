package dev.peppy.mobile

import org.junit.Assert.assertFalse
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class RelayWakeClientTest {
    @Test fun challengeConfirmationPublishesOnlyRouteAndWakeCredential() {
        val transport = object : RelayWakeTransport {
            override fun post(path: String, body: String) = when (path) {
                "/v1/registrations" -> HttpResult(201, "{\"registration_id\":\"00000000-0000-4000-8000-000000000001\"}")
                else -> HttpResult(201, "{\"route_id\":\"00000000-0000-4000-8000-000000000002\",\"manage_credential\":\"${"a".repeat(64)}\",\"wake_credential\":\"${"b".repeat(64)}\"}")
            }
        }
        var stored: RelayRoute? = null
        val store = object : RelayRouteStore {
            override fun route() = stored
            override fun save(route: RelayRoute) { stored = route }
            override fun clear() { stored = null }
            var pending: String? = null
            override fun pendingRegistration() = pending
            override fun savePendingRegistration(id: String) { pending = id }
            override fun clearPendingRegistration() { pending = null }
        }
        var published: Pair<String, String>? = null
        val client = RelayWakeClient(transport, store, RelayRoutePublisher { route, wake -> published = route to wake })
        assertEquals("00000000-0000-4000-8000-000000000001", client.register("provider-token"))
        assertTrue(client.confirm("f".repeat(64)))
        assertEquals("00000000-0000-4000-8000-000000000002" to "${"b".repeat(64)}", published)
        assertEquals("${"a".repeat(64)}", stored!!.manageCredential)
    }

    @Test fun malformedChallengeIsRejectedBeforeTransport() {
        assertFalse(Regex("[0-9a-f]{64}", RegexOption.IGNORE_CASE).matches("not-a-challenge"))
    }
}
