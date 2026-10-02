package dev.openpush.mobile

import android.content.Context
import android.os.Build
import android.os.Bundle
import android.telephony.SmsManager

data class MmsLimits(val maximumPduBytes: Long, val maximumRecipients: Int, val enabled: Boolean, val source: Source) {
    enum class Source { CARRIER, FALLBACK }

    companion object {
        const val FALLBACK_PDU_BYTES = 300L * 1024
        fun forSubscription(context: Context, subscriptionId: Int, values: Bundle? = null): MmsLimits {
            val config = values ?: try {
                val manager = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) context.getSystemService(SmsManager::class.java).createForSubscriptionId(subscriptionId)
                else @Suppress("DEPRECATION") SmsManager.getSmsManagerForSubscriptionId(subscriptionId)
                manager.carrierConfigValues
            } catch (_: RuntimeException) { null }
            if (config == null || config.isEmpty) return MmsLimits(FALLBACK_PDU_BYTES, 20, true, Source.FALLBACK)
            val configuredBytes = config.getInt(SmsManager.MMS_CONFIG_MAX_MESSAGE_SIZE, 0).toLong()
            val bytes = configuredBytes.takeIf { it in 1..(32L * 1024 * 1024) } ?: FALLBACK_PDU_BYTES
            val recipients = config.getInt(SmsManager.MMS_CONFIG_RECIPIENT_LIMIT, 0).takeIf { it > 0 }?.coerceAtMost(20) ?: 20
            val enabled = !config.containsKey(SmsManager.MMS_CONFIG_MMS_ENABLED) || config.getBoolean(SmsManager.MMS_CONFIG_MMS_ENABLED, true)
            return MmsLimits(bytes, recipients, enabled, if (configuredBytes == bytes) Source.CARRIER else Source.FALLBACK)
        }
    }
}
