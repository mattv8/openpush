package dev.peppy.mobile

import android.app.Application
import com.google.firebase.messaging.FirebaseMessaging

/** Restores optional content-free relay handling before FCM delivers a restart wake. */
class PeppyApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        RelayWakeRuntime.initialize(this)
        if (RelayWakeRuntime.enabled && resources.getIdentifier("google_app_id", "string", packageName) != 0) {
            FirebaseMessaging.getInstance().token.addOnSuccessListener { token ->
                RelayWakeRuntime.client?.register(token)
            }
        }
    }
}
