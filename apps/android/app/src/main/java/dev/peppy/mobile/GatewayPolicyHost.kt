package dev.peppy.mobile

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import androidx.core.content.ContextCompat
import uniffi.peppy_mobile_bindings.MobileBindingsException
import uniffi.peppy_mobile_bindings.NativeGatewayHostFacts
import uniffi.peppy_mobile_bindings.NativeGatewayPlatform
import uniffi.peppy_mobile_bindings.NativeGatewayPolicyDecision
import uniffi.peppy_mobile_bindings.NativeGatewaySettings

/**
 * Android's transient host facts for the Rust-owned durable gateway policy.
 *
 * The old shared preferences are read only once to preserve an existing user's choices, then
 * removed. They are not a fallback policy store: when the core database is unavailable callers
 * must retain their existing safe/locked behavior instead of inventing a second policy.
 */
class GatewayPolicyHost(private val context: Context) {
    private val appContext = context.applicationContext

    var mirroringEnabled: Boolean
        get() = settings()?.mirroringEnabled ?: false
        set(value) = update { it.mirroringEnabled = value }
    var smsCaptureEnabled: Boolean
        get() = settings()?.smsSyncEnabled ?: true
        set(value) = update { it.smsSyncEnabled = value }
    var mirroringWifiOnly: Boolean
        get() = settings()?.mirroringWifiOnly ?: false
        set(value) = update { it.mirroringWifiOnly = value }
    var skipSilent: Boolean
        get() = settings()?.skipSilent ?: true
        set(value) = update { it.skipSilent = value }
    var mediaWifiOnly: Boolean
        get() = settings()?.mediaWifiOnly ?: true
        set(value) = update { it.mediaWifiOnly = value }

    fun decision(notificationIsSilent: Boolean = false, smsAvailable: Boolean = hasReceiveSmsPermission()): NativeGatewayPolicyDecision? {
        val client = NativeGateway.open(appContext) ?: return null
        return try {
            migrateLegacySettings(client, client.gatewaySettings())
            client.gatewayPolicyDecision(
                NativeGatewayPlatform.ANDROID,
                NativeGatewayHostFacts(
                    onWifi(appContext),
                    NotificationMirrorAccess.granted(appContext),
                    smsAvailable,
                    notificationIsSilent,
                ),
            )
        } catch (_: MobileBindingsException) {
            null
        }
    }

    /** SMS's already-durable locked path must not be discarded just because policy lookup failed. */
    fun permitsSmsCapture(): Boolean = decision()?.captureSms ?: true

    private fun settings(): NativeGatewaySettings? {
        val client = NativeGateway.open(appContext) ?: return null
        return try {
            migrateLegacySettings(client, client.gatewaySettings())
            client.gatewaySettings()
        } catch (_: MobileBindingsException) {
            null
        }
    }

    private fun update(change: (NativeGatewaySettings) -> Unit) {
        val client = NativeGateway.open(appContext) ?: return
        try {
            val settings = client.gatewaySettings()
            migrateLegacySettings(client, settings)
            change(settings)
            client.setGatewaySettings(settings)
        } catch (_: MobileBindingsException) {
            // Database errors do not create a parallel native policy store.
        }
    }

    private fun migrateLegacySettings(client: uniffi.peppy_mobile_bindings.NativeClientInterface, settings: NativeGatewaySettings) {
        val migration = appContext.getSharedPreferences(MIGRATION_NAME, Context.MODE_PRIVATE)
        if (migration.getBoolean(MIGRATION_DONE, false)) return
        val legacyMirror = appContext.getSharedPreferences(LEGACY_MIRROR_NAME, Context.MODE_PRIVATE)
        val legacyHost = appContext.getSharedPreferences(LEGACY_HOST_NAME, Context.MODE_PRIVATE)
        // `master` defaulted off. Import it even when absent so an upgrade cannot inherit a
        // different device's state; this app's enrollment identity is immutable.
        settings.mirroringEnabled = legacyMirror.getBoolean(LEGACY_MASTER, false)
        if (legacyHost.contains(LEGACY_SMS_CAPTURE)) settings.smsSyncEnabled = legacyHost.getBoolean(LEGACY_SMS_CAPTURE, true)
        if (legacyHost.contains(LEGACY_MIRROR_WIFI)) settings.mirroringWifiOnly = legacyHost.getBoolean(LEGACY_MIRROR_WIFI, false)
        if (legacyHost.contains(LEGACY_SKIP_SILENT)) settings.skipSilent = legacyHost.getBoolean(LEGACY_SKIP_SILENT, false)
        if (legacyHost.contains(LEGACY_MEDIA_WIFI)) settings.mediaWifiOnly = legacyHost.getBoolean(LEGACY_MEDIA_WIFI, false)
        client.setGatewaySettings(settings)
        if (migration.edit().putBoolean(MIGRATION_DONE, true).commit()) {
            legacyMirror.edit().remove(LEGACY_MASTER).apply()
            legacyHost.edit().clear().apply()
        }
    }

    private fun hasReceiveSmsPermission() = ContextCompat.checkSelfPermission(appContext, Manifest.permission.RECEIVE_SMS) == PackageManager.PERMISSION_GRANTED

    companion object {
        private const val LEGACY_MIRROR_NAME = "peppy-notification-mirroring"
        private const val LEGACY_MASTER = "master"
        private const val LEGACY_HOST_NAME = "peppy-gateway-host-policy"
        private const val MIGRATION_NAME = "peppy-gateway-policy-migration"
        private const val MIGRATION_DONE = "legacy-imported-v1"
        private const val LEGACY_SMS_CAPTURE = "sms-capture"
        private const val LEGACY_MIRROR_WIFI = "mirror-wifi"
        private const val LEGACY_SKIP_SILENT = "skip-silent"
        private const val LEGACY_MEDIA_WIFI = "media-wifi"

        /** Wi-Fi means Android's Wi-Fi transport, never merely an unmetered network. */
        fun onWifi(context: Context): Boolean {
            val manager = context.getSystemService(ConnectivityManager::class.java) ?: return false
            return manager.getNetworkCapabilities(manager.activeNetwork)
                ?.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) == true
        }
    }
}
