package dev.peppy.mobile

import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage

/** FCM receives only `kind`/`value`; no envelope plaintext is accepted or displayed. */
class PeppyMessagingService : FirebaseMessagingService() {
    override fun onNewToken(token: String) {
        RelayWakeRuntime.initialize(applicationContext)
        if (RelayWakeRuntime.enabled) RelayWakeRuntime.client?.register(token)
    }

    override fun onMessageReceived(message: RemoteMessage) {
        RelayWakeRuntime.initialize(applicationContext)
        if (!RelayWakeRuntime.enabled) return
        when (message.data["kind"]) {
            "wake" -> GatewayWork.enqueue(applicationContext)
            "challenge" -> {
                val value = message.data["value"] ?: return
                RelayWakeRuntime.client?.confirm(value)
            }
        }
    }
}
