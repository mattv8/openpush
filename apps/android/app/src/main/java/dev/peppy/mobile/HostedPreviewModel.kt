package dev.peppy.mobile

import android.content.Context
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlinx.coroutines.Job
import org.json.JSONObject
import uniffi.peppy_mobile_bindings.hostedPreviewAdvance
import uniffi.peppy_mobile_bindings.hostedPreviewPassphrase
import uniffi.peppy_mobile_bindings.hostedPreviewPassphraseAcceptable
import uniffi.peppy_mobile_bindings.hostedPreviewResume
import uniffi.peppy_mobile_bindings.hostedPreviewStart

/** Debug-only boundary around Rust policy. It never reaches NativeGateway. */
internal class HostedPreviewModel(
    context: Context,
    private val auth: PreviewAuthProvider = FakePreviewAuthProvider,
    private val purchase: PreviewPurchaseProvider = FakePreviewPurchaseProvider,
    private val hostedService: PreviewHostedService = FakePreviewHostedService,
) {
    private val preferences = context.getSharedPreferences("hosted-preview-checkpoint.v1", Context.MODE_PRIVATE)
    var snapshot by mutableStateOf(preferences.getString("snapshot", null)?.let(::resume) ?: start("new"))
        private set
    var passphrase by mutableStateOf("")
        private set
    internal var accountId by mutableStateOf<String?>(
        if (JSONObject(snapshot).getString("account_state") in setOf("signed_in", "new_account", "existing")) "preview-account-linked" else null,
    )
    var localError by mutableStateOf<String?>(null)
        private set
    private var pendingPurchase: PreviewPurchaseResult? = null
    internal var provisionJob by mutableStateOf<Job?>(null)

    val screen: String get() = JSONObject(snapshot).getString("screen")
    val statusKey: String? get() = JSONObject(snapshot).let { if (it.isNull("status_key")) null else it.optString("status_key").takeIf { s -> s.isNotEmpty() } }
    val scenario: String get() = JSONObject(snapshot).getString("scenario")
    val entitlement: String get() = JSONObject(snapshot).getString("entitlement_state")
    val operationId: String? get() = JSONObject(snapshot).let { if (it.isNull("operation_id")) null else it.optString("operation_id").takeIf { s -> s.isNotEmpty() } }
    val hasResumableEntitlement: Boolean get() = entitlement in setOf("active", "grace", "billing_retry")
    val displayPrice: String get() = purchase.displayPrice()

    fun chooseScenario(scenario: String) { cancelProvision(); clearSecret(); accountId = null; pendingPurchase = null; localError = null; snapshot = start(scenario) }
    fun advance(event: String) {
        // Cancel provision on cancel/back/signout/reset
        if (event in setOf("cancel", "back", "signout", "reset", "deletion_confirmed")) cancelProvision()
        localError = null
        snapshot = hostedPreviewAdvance(snapshot, event).also(::save)
        // Clear account on signout/reset, and clear secrets on cancel/back/signout/reset
        if (event in setOf("cancel", "back", "signout", "reset", "deletion_confirmed")) clearSecret()
        if (event in setOf("signout", "reset", "deletion_confirmed")) { accountId = null; pendingPurchase = null }
    }
    fun signIn(provider: String) { accountId = auth.signIn(provider); advance("signed_in") }
    fun purchase() = completePurchase(restoring = false)
    fun restore() = completePurchase(restoring = true)
    private fun completePurchase(restoring: Boolean) {
        if (statusKey == "hosted_subscribe_store_unavailable") return
        val id = accountId ?: run { localError = "preview_error"; return }
        if (hasResumableEntitlement) {
            advance("restore_succeeded")
            if (screen == "passphrase" && passphrase.isEmpty()) generatePassphrase()
            return
        }
        val result = if (restoring) purchase.restoreResult(id) else purchase.purchaseResult(id)
        if (result.accountId != id) { localError = "preview_error"; return }
        pendingPurchase = result
        advance(result.event)
    }
    fun retryStore() = advance("store_retry")
    fun verifyEntitlement() {
        if (screen != "subscription_verifying") return
        val id = accountId ?: run { localError = "preview_error"; return }
        val result = pendingPurchase ?: purchase.restoreResult(id)
        if (result.accountId != id) { localError = "preview_error"; return }
        if (entitlement != "verifying") advance("verify_entitlement")
        advance(hostedService.verifyEntitlement(id, result))
        if (screen == "passphrase" && passphrase.isEmpty()) generatePassphrase()
    }
    fun provision() {
        if (screen == "provisioning" && statusKey == null) operationId?.let { advance(hostedService.provision(it)) }
    }
    fun approval() = advance(hostedService.approvalResult())
    fun generatePassphrase() { passphrase = hostedPreviewPassphrase() }
    fun createPassphrase(value: String): Boolean {
        if (!hostedPreviewPassphraseAcceptable(value)) return false
        passphrase = value
        advance("local_passphrase_accepted")
        return true
    }
    fun confirmCreation(value: String): Boolean {
        if (value != passphrase) return false
        advance("passphrase_confirmed")
        clearSecret()
        return true
    }
    fun unlock(value: String): Boolean {
        if (value.isEmpty()) return false
        // Existing phrases are not subjected to new-account strength rules in this simulation.
        advance("passphrase_confirmed")
        clearSecret()
        return true
    }
    fun clearSecret() { passphrase = "" }
    fun cancelProvision() { provisionJob?.cancel(); provisionJob = null }
    fun resumeAfterInterruption() { cancelProvision(); clearSecret(); snapshot = hostedPreviewResume(snapshot).also(::save) }
    fun reset() { cancelProvision(); clearSecret(); accountId = null; pendingPurchase = null; localError = null; preferences.edit().clear().apply(); snapshot = start("new") }

    private fun start(scenario: String): String = hostedPreviewStart(scenario).also(::save)
    private fun resume(saved: String): String = hostedPreviewResume(saved).also(::save)
    private fun save(value: String): String { preferences.edit().putString("snapshot", value).apply(); return value }
}

/** Stand-in seams intentionally carry no credential, passphrase, proof, or gateway object. */
internal interface PreviewAuthProvider { fun signIn(provider: String): String }
internal data class PreviewPurchaseResult(val event: String, val accountId: String)
internal interface PreviewPurchaseProvider {
    fun purchaseResult(accountId: String): PreviewPurchaseResult
    fun restoreResult(accountId: String): PreviewPurchaseResult
    fun displayPrice(): String
}
internal interface PreviewHostedService {
    fun provision(operationId: String): String
    fun approvalResult(): String
    fun verifyEntitlement(accountId: String, purchase: PreviewPurchaseResult): String
}
internal object FakePreviewAuthProvider : PreviewAuthProvider { override fun signIn(provider: String) = "preview-account-linked" }
internal object FakePreviewPurchaseProvider : PreviewPurchaseProvider {
    override fun purchaseResult(accountId: String) = PreviewPurchaseResult("purchase_succeeded", accountId)
    override fun restoreResult(accountId: String) = PreviewPurchaseResult("restore_succeeded", accountId)
    override fun displayPrice() = "$4.99/month · Preview price"
}
internal object FakePreviewHostedService : PreviewHostedService {
    override fun provision(operationId: String) = "provision_finished"
    override fun approvalResult() = "approval_granted"
    override fun verifyEntitlement(accountId: String, purchase: PreviewPurchaseResult) =
        if (purchase.accountId == accountId && purchase.event in setOf("purchase_succeeded", "restore_succeeded")) "entitlement_verified" else "entitlement_rejected"
}
