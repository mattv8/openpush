package dev.peppy.mobile

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.telephony.PhoneNumberUtils
import android.telephony.SubscriptionManager
import android.telephony.TelephonyManager

internal sealed interface OwnNumberDetection {
    data class Detected(val number: String) : OwnNumberDetection
    data object NoAccess : OwnNumberDetection
    data object Unavailable : OwnNumberDetection
}

internal fun interface OwnNumberSource {
    fun read(subscriptionId: Int): String?
}

/** Reads a carrier-provided line number and commits it only to the encrypted native store. */
internal object OwnNumberDetector {
    private val PHONE_NUMBER = Regex("^\\+?\\d{7,15}$")

    fun hasAccess(context: Context?): Boolean = context != null &&
        (context.checkSelfPermission(Manifest.permission.READ_PHONE_NUMBERS) == PackageManager.PERMISSION_GRANTED ||
            context.checkSelfPermission(Manifest.permission.READ_SMS) == PackageManager.PERMISSION_GRANTED)

    fun normalizeOwnNumber(
        raw: String?,
        countryIso: String?,
        toE164: (String, String) -> String? = { number, country -> PhoneNumberUtils.formatNumberToE164(number, country) },
    ): String? = try {
        val number = raw?.trim()?.takeIf { it.isNotEmpty() } ?: return null
        val formatted = try {
            countryIso?.trim()?.takeIf { it.isNotEmpty() }?.let { country ->
                toE164(number, country.uppercase())
            }
        } catch (_: Exception) {
            null
        }
        val fallback = number.takeIf { candidate ->
            candidate.withIndex().all { (index, character) ->
                character.isDigit() || character in " -()." || (character == '+' && index == 0)
            }
        }?.filter { it.isDigit() || it == '+' }
        val candidate = formatted?.takeIf { PHONE_NUMBER.matches(it) } ?: fallback
        candidate?.takeIf { PHONE_NUMBER.matches(it) }
    } catch (_: Exception) {
        null
    }

    fun detect(
        context: Context?,
        route: SimRoute,
        source: OwnNumberSource = androidSource(context),
        access: (Context?) -> Boolean = ::hasAccess,
        countryIso: (Int) -> String? = androidCountryIso(context),
        toE164: (String, String) -> String? = { number, country -> PhoneNumberUtils.formatNumberToE164(number, country) },
    ): OwnNumberDetection {
        return try {
            if (!access(context)) OwnNumberDetection.NoAccess else {
                normalizeOwnNumber(source.read(route.subscriptionId), countryIso(route.subscriptionId), toE164)
                    ?.let(OwnNumberDetection::Detected) ?: OwnNumberDetection.Unavailable
            }
        } catch (_: SecurityException) {
            OwnNumberDetection.Unavailable
        } catch (_: IllegalStateException) {
            OwnNumberDetection.Unavailable
        } catch (_: UnsupportedOperationException) {
            OwnNumberDetection.Unavailable
        }
    }

    internal fun sync(
        route: SimRoute?,
        detect: (SimRoute) -> OwnNumberDetection,
        saved: (String) -> String?,
        writer: (String, String) -> Unit,
    ): OwnNumberDetection? {
        val currentRoute = route ?: return null
        val result = try {
            detect(currentRoute)
        } catch (_: Exception) {
            return OwnNumberDetection.Unavailable
        }
        try {
            seedDetectedNumber(saved(currentRoute.routeId), result) { number ->
                writer(currentRoute.routeId, number)
            }
        } catch (_: Exception) {
            // This is best-effort metadata; capture must continue without it.
        }
        return result
    }

    internal fun seedDetectedNumber(
        saved: String?,
        detection: OwnNumberDetection,
        writer: (String) -> Unit,
    ): String? {
        if (saved != null) return saved
        val detected = detection as? OwnNumberDetection.Detected ?: return null
        writer(detected.number)
        return detected.number
    }

    fun syncCurrentRoute(context: Context): OwnNumberDetection? = try {
        val client = NativeGateway.open(context) ?: return null
        sync(
            route = SimRoutes.current().singleOrNull(),
            detect = { route -> detect(context, route) },
            saved = { routeId -> client.mmsOwnAddress(routeId) },
        ) { routeId, number -> client.setMmsOwnAddress(routeId, number) }
    } catch (_: Exception) {
        null
    }

    private fun androidSource(context: Context?): OwnNumberSource = OwnNumberSource { subscriptionId ->
        if (context == null) return@OwnNumberSource null
        val phoneNumber = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            context.checkSelfPermission(Manifest.permission.READ_PHONE_NUMBERS) == PackageManager.PERMISSION_GRANTED
        ) {
            context.getSystemService(SubscriptionManager::class.java)?.getPhoneNumber(subscriptionId)
        } else {
            null
        }
        phoneNumber?.takeIf { it.isNotBlank() } ?: line1Number(context, subscriptionId)
    }

    @Suppress("DEPRECATION", "MissingPermission")
    private fun line1Number(context: Context, subscriptionId: Int): String? =
        context.getSystemService(TelephonyManager::class.java)
            ?.createForSubscriptionId(subscriptionId)
            ?.line1Number

    private fun androidCountryIso(context: Context?): (Int) -> String? = { subscriptionId ->
        context?.getSystemService(TelephonyManager::class.java)
            ?.createForSubscriptionId(subscriptionId)
            ?.simCountryIso
    }
}
