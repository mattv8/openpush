package dev.openpush.mobile

import android.telephony.SubscriptionManager
import org.json.JSONArray
import org.json.JSONObject

/** One explicitly addressable carrier route published to the vault. */
data class SimRoute(val routeId: String, val subscriptionId: Int, val label: String)

/**
 * Explicit SIM routes. A route ID is derived only from the Android subscription ID of the current
 * default SMS subscription. The platform normally allocates a new subscription record for a newly
 * inserted card, but that is not a documented guarantee and this build cannot verify card identity
 * (no READ_PHONE_STATE). A command is therefore honoured only while its route equals the current
 * default SMS subscription; if the user changes the default or removes the SIM, commands for the
 * old route wait (no permit, no carrier call) and are never redirected to another SIM.
 */
object SimRoutes {
    private const val PREFIX = "android-subscription-"

    fun routeId(subscriptionId: Int) = "$PREFIX$subscriptionId"

    fun current(defaultSmsSubscriptionId: Int = SubscriptionManager.getDefaultSmsSubscriptionId()): List<SimRoute> =
        if (defaultSmsSubscriptionId == SubscriptionManager.INVALID_SUBSCRIPTION_ID || defaultSmsSubscriptionId < 0) {
            emptyList()
        } else {
            listOf(SimRoute(routeId(defaultSmsSubscriptionId), defaultSmsSubscriptionId, "Default SMS SIM"))
        }

    /** Exact match only; there is no fallback to another or a default SIM. */
    fun resolve(routeId: String, current: List<SimRoute>): SimRoute? = current.firstOrNull { it.routeId == routeId }

    /**
     * The exact `POST /v1/capabilities` body consumed by desktop `gateways.rs`:
     * `{"simulator":false,"capabilities":{"sims":[{"subscription_id","label","sms","mms"}]}}`.
     * MMS is always `unsupported` in this checkpoint. An empty `sims` array is published on
     * purpose when no SIM is usable so a previous report cannot leave a stale route selectable.
     */
    fun capabilityReport(routes: List<SimRoute>, sendPermissionGranted: Boolean): JSONObject {
        val sims = JSONArray()
        routes.take(MAX_SIMS).forEach { route ->
            sims.put(
                JSONObject()
                    .put("subscription_id", route.routeId)
                    .put("label", route.label)
                    .put("sms", if (sendPermissionGranted) "available" else "permission_required")
                    .put("mms", "unsupported"),
            )
        }
        return JSONObject().put("simulator", false).put("capabilities", JSONObject().put("sims", sims))
    }

    private const val MAX_SIMS = 8
}
