package dev.peppy.mobile

import android.os.Bundle
import android.telephony.SmsManager
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class MmsLimitsTest {
    @Test fun conservativeFallbackIsUsedForUnknownOrUnsafeValues() {
        val limits = MmsLimits.forSubscription(RuntimeEnvironment.getApplication(), 1, Bundle().apply { putInt(SmsManager.MMS_CONFIG_MAX_MESSAGE_SIZE, Int.MAX_VALUE) })
        assertEquals(MmsLimits.FALLBACK_PDU_BYTES, limits.maximumPduBytes)
        assertEquals(MmsLimits.Source.FALLBACK, limits.source)
    }

    @Test fun carrierLimitsAreClampedAndDisabledStateIsExposed() {
        val limits = MmsLimits.forSubscription(RuntimeEnvironment.getApplication(), 1, Bundle().apply {
            putInt(SmsManager.MMS_CONFIG_MAX_MESSAGE_SIZE, 1024); putInt(SmsManager.MMS_CONFIG_RECIPIENT_LIMIT, 99); putBoolean(SmsManager.MMS_CONFIG_MMS_ENABLED, false)
        })
        assertEquals(1024, limits.maximumPduBytes); assertEquals(20, limits.maximumRecipients); assertFalse(limits.enabled)
    }

    @Test fun emptyConfigIsAnUnknownApplicationFallback() {
        val limits = MmsLimits.forSubscription(RuntimeEnvironment.getApplication(), 1, Bundle())
        assertEquals(MmsLimits.Source.FALLBACK, limits.source); assertTrue(limits.enabled)
    }
}
