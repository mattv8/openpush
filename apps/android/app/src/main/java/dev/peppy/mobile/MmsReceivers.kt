package dev.peppy.mobile

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** WAP notifications are only a reconciliation hint; carrier content is never fetched here. */
class MmsNotificationReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (!isMmsWapPush(intent)) return
        MmsCaptureWork.enqueue(context.applicationContext)
    }

    companion object {
        const val WAP_PUSH_RECEIVED = "android.provider.Telephony.WAP_PUSH_RECEIVED"
        const val MMS_MIME = "application/vnd.wap.mms-message"
        internal fun isMmsWapPush(intent: Intent) = intent.action == WAP_PUSH_RECEIVED && intent.type == MMS_MIME
    }
}
