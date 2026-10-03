package dev.peppy.mobile

import android.Manifest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class GatewayPolicyHostTest : GatewayTestBase() {
    @Test
    fun disabledSmsPolicyDoesNotReachNewDispatch() {
        grant(Manifest.permission.SEND_SMS)
        val client = enrollAndUnlock()
        val settings = client.gatewaySettings()
        settings.smsSyncEnabled = false
        client.setGatewaySettings(settings)

        var dispatched = false
        val result = GatewayDispatchPolicy.sms(GatewayPolicyHost(context).decision(smsAvailable = true)?.captureSms == true) {
            dispatched = true
            DispatchSummary(1, 0, 0, false)
        }
        assertFalse(dispatched)
        assertEquals(0, result.submitted)
    }

    @Test
    fun syntheticTestCaptureIsOneMarkedRecordAndRejectsLockedOrPolicyStates() {
        assertEquals(NotificationTestCaptureResult.LOCKED, NotificationTestCapture.capture(null, permitted = true, enqueue = {}))
        val client = enrollAndUnlock()
        assertEquals(NotificationTestCaptureResult.POLICY_BLOCKED, NotificationTestCapture.capture(client, permitted = false, enqueue = {}))

        var enqueues = 0
        assertEquals(NotificationTestCaptureResult.CAPTURED, NotificationTestCapture.capture(client, permitted = true, enqueue = { enqueues++ }, id = "one"))
        val snapshot = client.notificationSnapshot().notifications
        assertEquals(1, snapshot.size)
        assertEquals(NotificationTestCapture.SYNTHETIC_KEY, snapshot.single().target.notificationKey)
        assertEquals(1, enqueues)
        assertEquals(NotificationTestCaptureResult.ALREADY_QUEUED, NotificationTestCapture.capture(client, permitted = true, enqueue = { enqueues++ }, id = "two"))
        assertEquals(1, client.notificationSnapshot().notifications.size)
        assertTrue(!NotificationReconciliation.shouldRemoveAbsentSnapshot(NotificationTestCapture.SYNTHETIC_KEY))
    }
}
