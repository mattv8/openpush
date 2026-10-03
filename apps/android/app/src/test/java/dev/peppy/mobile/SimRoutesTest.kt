package dev.peppy.mobile

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class SimRoutesTest {
    @Test
    fun routesAreStableExplicitAndNeverFallBack() {
        val routes = SimRoutes.current(defaultSmsSubscriptionId = 7)
        assertEquals(listOf(SimRoute("android-subscription-7", 7, "Default SMS SIM")), routes)
        assertEquals(routes, SimRoutes.current(defaultSmsSubscriptionId = 7))
        assertEquals(7, SimRoutes.resolve("android-subscription-7", routes)?.subscriptionId)
        assertNull(SimRoutes.resolve("android-subscription-8", routes))
        assertNull(SimRoutes.resolve("", routes))
        assertTrue(SimRoutes.current(defaultSmsSubscriptionId = -1).isEmpty())
    }

    @Test
    fun capabilityReportMatchesDesktopGatewayContract() {
        val routes = SimRoutes.current(defaultSmsSubscriptionId = 7)
        assertEquals(
            """{"simulator":false,"capabilities":{"sims":[{"subscription_id":"android-subscription-7","label":"Default SMS SIM","sms":"available","mms":"unsupported"}]}}""",
            SimRoutes.capabilityReport(routes, sendPermissionGranted = true).toString(),
        )
        val denied = SimRoutes.capabilityReport(routes, sendPermissionGranted = false, mmsEnabled = true)
            .getJSONObject("capabilities").getJSONArray("sims").getJSONObject(0)
        assertEquals("permission_required", denied.getString("sms"))
        assertEquals("unsupported", denied.getString("mms"))
        assertEquals("permission_required", denied.getString("mms_receive"))
        val enabled = SimRoutes.capabilityReport(routes, sendPermissionGranted = true, mmsEnabled = true, mmsReceivePermissionGranted = true)
            .getJSONObject("capabilities").getJSONArray("sims").getJSONObject(0)
        assertEquals("available", enabled.getString("mms"))
        assertEquals(2, enabled.getInt("mms_content_version"))
        assertEquals("available", enabled.getString("mms_receive"))
        assertEquals(false, enabled.getBoolean("mms_carrier_verified"))
        assertEquals(
            """{"simulator":false,"capabilities":{"sims":[]}}""",
            SimRoutes.capabilityReport(emptyList(), sendPermissionGranted = true).toString(),
        )
    }
}
