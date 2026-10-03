package dev.peppy.mobile

import android.app.Notification
import android.content.Context
import android.service.notification.StatusBarNotification
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class NotificationExtractionTest {
    private val context: Context = ApplicationProvider.getApplicationContext()

    private fun sbn(notification: Notification, postTime: Long = 42L) = StatusBarNotification(
        "example.chat", "example.chat", 1, "tag", 1000, 0, 0, notification, android.os.Process.myUserHandle(), postTime,
    )

    @Test fun extractsPublicTitleTextAndStableInstance() {
        val notification = Notification.Builder(context, "test")
            .setContentTitle("Title").setContentText("Text").build()
        val captured = checkNotNull(NotificationExtraction.extract(context, sbn(notification)))
        assertEquals("Title", captured.title)
        assertEquals("Text", captured.text)
        assertTrue(captured.instance.endsWith(":42"))
    }

    @Test fun retainsListenerSilentRankingFactForPolicyEvaluation() {
        val notification = Notification.Builder(context, "test").setContentText("Text").build()
        assertTrue(checkNotNull(NotificationExtraction.extract(context, sbn(notification), isSilent = true)).isSilent)
    }

    @Test fun prefersNonemptyBigTextAndBoundsUtf8WithoutBrokenSurrogates() {
        val notification = Notification.Builder(context, "test").setContentTitle("😀".repeat(2000))
            .setContentText("").setStyle(Notification.BigTextStyle().bigText("expanded")).build()
        val captured = checkNotNull(NotificationExtraction.extract(context, sbn(notification)))
        assertEquals("expanded", captured.text)
        assertTrue(captured.title.toByteArray(Charsets.UTF_8).size + captured.text.toByteArray(Charsets.UTF_8).size <= 4096)
        assertEquals("x", NotificationExtraction.utf8Truncate("x\uD800", 10))
    }

    @Test fun excludesOngoingGroupSummaryAndEmptyNotifications() {
        assertNull(NotificationExtraction.extract(context, sbn(Notification.Builder(context, "test").setOngoing(true).setContentText("x").build())))
        assertNull(NotificationExtraction.extract(context, sbn(Notification.Builder(context, "test").setGroupSummary(true).setContentText("x").build())))
        assertNull(NotificationExtraction.extract(context, sbn(Notification.Builder(context, "test").build())))
    }

    @Test fun masterDefaultsOff() {
        context.getSharedPreferences("peppy-notification-mirroring", Context.MODE_PRIVATE).edit().clear().commit()
        assertTrue(!NotificationMirrorPreferences.enabled(context))
    }

    @Test fun removingOldInstanceIsNotBlockedByImmediateSameKeyRepost() {
        val notification = Notification.Builder(context, "test").setContentText("body").build()
        val old = sbn(notification, 42)
        val repost = sbn(notification, 43)
        assertTrue(NotificationExtraction.removedInstanceStillActive(old, arrayOf(old)))
        assertTrue(!NotificationExtraction.removedInstanceStillActive(old, arrayOf(repost)))
    }
}
