package dev.peppy.mobile

import android.content.Context
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.isActive
import org.json.JSONObject
import uniffi.peppy_mobile_bindings.hostedPreviewPassphrase
import uniffi.peppy_mobile_bindings.hostedPreviewPassphraseAcceptable
import uniffi.peppy_mobile_bindings.hostedPreviewV2Advance
import uniffi.peppy_mobile_bindings.hostedPreviewV2Resume
import uniffi.peppy_mobile_bindings.hostedPreviewV2Start

/** Preview-only host for the Rust-owned v2 reducer. It never touches NativeGateway or OS data. */
internal class HostedPreviewModel(
    context: Context,
    private val auth: PreviewAuthProvider = FakePreviewAuthProvider(),
    private val purchase: PreviewPurchaseProvider = FakePreviewPurchaseProvider(),
    private val accountLookup: PreviewAccountLookupProvider = FakePreviewAccountLookupProvider(),
    private val preparation: PreviewPreparationProvider = FakePreviewPreparationProvider(),
    private val hostedService: PreviewHostedService = FakePreviewHostedService(),
    private val storage: PreviewCheckpointStorage = SharedPreferencesCheckpointStorage(context),
    private val initialScenario: String = "new",
) {
    var snapshot by mutableStateOf(loadSnapshot())
        private set
    var passphrase by mutableStateOf("")
        private set
    var accountId by mutableStateOf<String?>(null)
        private set
    var signInProvider by mutableStateOf<String?>(null)
        private set
    var localError by mutableStateOf<String?>(null)
        private set
    private var pendingPurchase: PreviewPurchaseResult? = null
    private var generation = 0L
    private var foregroundHandled = false
    private var busy by mutableStateOf(false)

    val screen get() = field("screen")
    val statusKey get() = nullableField("status_key")
    val scenario get() = field("scenario")
    val entitlement get() = field("entitlement_state")
    val accountState get() = field("account_state")
    val operationId get() = nullableField("operation_id")
    val hasResumableEntitlement get() = entitlement in setOf("active", "grace", "billing_retry")
    val isBusy get() = busy
    val displayPrice get() = purchase.displayPrice()

    fun advance(event: String) {
        val interrupting = event in setOf("cancel", "back", "signout", "reset", "deletion_confirmed")
        if (interrupting) invalidate()
        localError = null
        snapshot = hostedPreviewV2Advance(snapshot, event).also(::save)
        if (interrupting) clearSecret()
        if (event in setOf("signout", "reset", "deletion_confirmed")) {
            auth.signOut()
            clearSession()
        }
    }

    suspend fun signIn(provider: String) {
        val work = begin("signin") ?: return
        try {
            val session = auth.signIn(provider)
            if (!work.current("signin")) return
            if (session.provider != provider) {
                localError = "preview_identity_mismatch"
                return
            }
            bind(session)
            advance("signed_in")
            resolveAccount(work, session)
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: Exception) {
            when {
                work.current("checking_account") -> lookupFailed()
                work.current("signin") -> localError = "preview_error"
            }
        } finally {
            finish(work)
        }
    }

    /** Invoke once for each foreground entry; persisted snapshots never establish a session. */
    suspend fun onForeground() {
        if (foregroundHandled || screen !in resumableScreens) return
        foregroundHandled = true
        val work = begin(*resumableScreens.toTypedArray()) ?: return
        try {
            val session = auth.restore()
            if (!work.current(*resumableScreens.toTypedArray())) return
            if (session == null) {
                requireSignIn()
                return
            }
            when (screen) {
                "purchase_pending", "checking_account" -> advance("account_retry")
                "signin" -> advance("session_restored")
                else -> {
                    normalizeForAuthentication()
                    advance("session_restored")
                }
            }
            if (!work.current("checking_account")) return
            bind(session)
            resolveAccount(work, session)
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: Exception) {
            if (work.current("checking_account")) lookupFailed()
        } finally {
            finish(work)
        }
    }

    fun onBackground() {
        foregroundHandled = false
        invalidate()
        clearSecret()
        clearSession()
        normalizeForAuthentication()
    }

    suspend fun retryAccount() {
        val work = begin("checking_account", "purchase_pending") ?: return
        try {
            val session = currentSession() ?: run {
                requireSignIn()
                return
            }
            advance("account_retry")
            resolveAccount(work, session)
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: Exception) {
            if (work.current("checking_account")) lookupFailed()
        } finally {
            finish(work)
        }
    }

    fun purchase() {
        if (busy || screen != "subscribe" || statusKey == "hosted_subscribe_store_unavailable" || hasResumableEntitlement) return
        val session = currentSession() ?: return
        localError = null
        try {
            val result = purchase.purchaseResult(session)
            if (!result.matches(session)) {
                localError = "preview_identity_mismatch"
                return
            }
            pendingPurchase = result
            (accountLookup as? PreviewFixtureUpdater)?.recordReceipt(session, accountState == "existing", result.event)
            advance(result.event)
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: Exception) {
            localError = "preview_error"
        }
    }

    suspend fun retryStore() {
        val work = begin("subscribe") ?: return
        try {
            val session = currentSession() ?: run {
                requireSignIn()
                return
            }
            advance("store_retry")
            resolveAccount(work, session)
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: Exception) {
            if (work.current("checking_account")) lookupFailed()
        } finally {
            finish(work)
        }
    }

    fun verifyEntitlement() {
        if (busy || screen != "subscription_verifying" || localError != null) return
        val session = currentSession() ?: run {
            localError = "preview_error"
            return
        }
        val result = pendingPurchase ?: run {
            localError = "preview_error"
            return
        }
        if (!result.matches(session)) {
            localError = "preview_identity_mismatch"
            return
        }
        val existing = accountState == "existing"
        try {
            if (entitlement == "store_succeeded_unverified") advance("verify_entitlement")
            val verification = hostedService.verifyEntitlement(session, result)
            advance(verification)
            val updater = accountLookup as? PreviewFixtureUpdater
            if (verification == "entitlement_verified" && screen in setOf("passphrase", "join")) {
                updater?.recordEntitlement(session, existing)
                pendingPurchase = null
            } else if (verification == "entitlement_rejected") {
                updater?.recordEntitlementRejected(session, existing)
                pendingPurchase = null
            }
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: Exception) {
            localError = "preview_error"
        }
    }

    fun retryVerification() {
        if (screen != "subscription_verifying" || busy) return
        localError = null
        verifyEntitlement()
    }

    suspend fun prepare() {
        if (statusKey != null) return
        val work = begin("syncing") ?: return
        try {
            val session = currentSession() ?: return
            val result = preparation.prepare(session)
            if (!work.current("syncing")) return
            if (!result.matches(session)) {
                advance("sync_failed")
                localError = "preview_identity_mismatch"
                return
            }
            advance(result.event)
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: Exception) {
            if (work.current("syncing")) advance("sync_failed")
        } finally {
            finish(work)
        }
    }

    fun retryPreparation() {
        if (!busy && screen == "syncing" && statusKey == "hosted_data_prepare_failed") advance("sync_retry")
    }

    suspend fun provision() {
        val work = begin("provisioning") ?: return
        try {
            if (statusKey != null) return
            val session = currentSession() ?: return
            val operation = operationId ?: return
            val result = hostedService.provision(session, operation)
            if (!work.current("provisioning")) return
            if (!result.matches(session, operation)) {
                advance("provision_failed")
                localError = "preview_identity_mismatch"
                return
            }
            advance(result.event)
            if (screen == "permissions") (accountLookup as? PreviewFixtureUpdater)?.recordProvisioned(session)
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: Exception) {
            if (work.current("provisioning")) advance("provision_failed")
        } finally {
            finish(work)
        }
    }

    fun approval() = advance(hostedService.approvalResult())
    fun generatePassphrase() {
        if (screen == "passphrase" && hasResumableEntitlement) passphrase = hostedPreviewPassphrase()
    }
    fun createPassphrase(value: String): Boolean {
        if (!hostedPreviewPassphraseAcceptable(value)) return false
        passphrase = value
        advance("local_passphrase_accepted")
        return true
    }
    fun confirmCreation(value: String): Boolean {
        if (value != passphrase) return false
        advance("passphrase_confirmed")
        currentSession()?.let { session -> operationId?.let { (accountLookup as? PreviewFixtureUpdater)?.recordProvisioning(session, it) } }
        clearSecret()
        return true
    }
    fun unlock(value: String): Boolean {
        if (value.isEmpty()) return false
        advance("unlock_succeeded")
        clearSecret()
        return true
    }
    fun clearSecret() { passphrase = "" }

    private suspend fun resolveAccount(work: Work, session: PreviewSession) {
        val result = accountLookup.lookup(session)
        if (!work.current("checking_account")) return
        if (!result.matches(session)) {
            localError = "preview_identity_mismatch"
            lookupFailed()
            return
        }
        if (result.event in verificationEvents) {
            pendingPurchase = PreviewPurchaseResult("purchase_succeeded", session.accountId, session.provider)
        }
        advance(result.event)
    }

    private fun lookupFailed() {
        snapshot = hostedPreviewV2Advance(snapshot, "account_lookup_failed").also(::save)
    }

    private fun normalizeForAuthentication() {
        if (screen == "welcome") return
        snapshot = hostedPreviewV2Resume(snapshot).also(::save)
    }

    private fun requireSignIn() {
        normalizeForAuthentication()
        clearSession()
    }

    private fun bind(session: PreviewSession) {
        accountId = session.accountId
        signInProvider = session.provider
    }

    private fun currentSession(): PreviewSession? {
        val id = accountId ?: return null
        val provider = signInProvider ?: return null
        return PreviewSession(id, provider)
    }

    private data class Work(val generation: Long)
    private fun begin(vararg allowedScreens: String): Work? {
        if (busy || screen !in allowedScreens) return null
        busy = true
        return Work(++generation)
    }
    private suspend fun Work.current(vararg allowedScreens: String) =
        this@HostedPreviewModel.generation == generation && currentCoroutineContext().isActive && screen in allowedScreens
    private fun finish(work: Work) {
        if (generation == work.generation) busy = false
    }
    private fun invalidate() {
        generation++
        busy = false
    }
    private fun clearSession() {
        accountId = null
        signInProvider = null
        pendingPurchase = null
    }
    private fun field(key: String) = JSONObject(snapshot).getString(key)
    private fun nullableField(key: String) = JSONObject(snapshot).let { if (it.isNull(key)) null else it.optString(key).ifEmpty { null } }
    private fun loadSnapshot(): String {
        val saved = storage.v2() ?: storage.v1()
        return (saved?.let(::hostedPreviewV2Resume) ?: hostedPreviewV2Start(initialScenario)).also {
            (accountLookup as? PreviewFixtureSeeder)?.seed(JSONObject(it))
            save(it)
        }
    }
    private fun save(value: String): String {
        storage.saveV2(value)
        return value
    }

    private companion object {
        val resumableScreens = setOf("signin", "checking_account", "purchase_pending", "subscription_verifying", "passphrase", "provisioning", "join", "unlock", "syncing", "lapsed")
        val verificationEvents = setOf("account_verification_pending", "account_existing_verification_pending")
    }
}

internal interface PreviewCheckpointStorage { fun v2(): String?; fun v1(): String?; fun saveV2(snapshot: String) }
internal class SharedPreferencesCheckpointStorage(context: Context) : PreviewCheckpointStorage {
    private val v2 = context.getSharedPreferences("hosted-preview-checkpoint.v2", Context.MODE_PRIVATE)
    private val v1 = context.getSharedPreferences("hosted-preview-checkpoint.v1", Context.MODE_PRIVATE)
    override fun v2() = v2.getString("snapshot", null)
    override fun v1() = v1.getString("snapshot", null)
    override fun saveV2(snapshot: String) { v2.edit().putString("snapshot", snapshot).apply() }
}

internal data class PreviewSession(val accountId: String, val provider: String)
internal data class PreviewAccountResult(val event: String, val accountId: String, val signInProvider: String) {
    fun matches(session: PreviewSession) = accountId == session.accountId && signInProvider == session.provider
}
internal interface PreviewAuthProvider { suspend fun signIn(provider: String): PreviewSession; suspend fun restore(): PreviewSession?; fun signOut() }
internal interface PreviewAccountLookupProvider { suspend fun lookup(session: PreviewSession): PreviewAccountResult }
internal interface PreviewFixtureSeeder { fun seed(validatedSnapshot: JSONObject) }
internal interface PreviewFixtureUpdater {
    fun recordReceipt(session: PreviewSession, existing: Boolean, purchaseEvent: String)
    fun recordEntitlement(session: PreviewSession, existing: Boolean)
    fun recordEntitlementRejected(session: PreviewSession, existing: Boolean)
    fun recordProvisioning(session: PreviewSession, operationId: String)
    fun recordProvisioned(session: PreviewSession)
}
internal data class PreviewPurchaseResult(val event: String, val accountId: String, val signInProvider: String) {
    fun matches(session: PreviewSession) = accountId == session.accountId && signInProvider == session.provider
}
internal interface PreviewPurchaseProvider { fun purchaseResult(session: PreviewSession): PreviewPurchaseResult; fun displayPrice(): String }
internal data class PreviewPreparationResult(val event: String, val accountId: String, val signInProvider: String) {
    fun matches(session: PreviewSession) = accountId == session.accountId && signInProvider == session.provider
}
internal interface PreviewPreparationProvider { suspend fun prepare(session: PreviewSession): PreviewPreparationResult }
internal data class PreviewProvisionResult(val event: String, val accountId: String, val signInProvider: String, val operationId: String) {
    fun matches(session: PreviewSession, operation: String) = accountId == session.accountId && signInProvider == session.provider && operationId == operation
}
internal interface PreviewHostedService {
    suspend fun provision(session: PreviewSession, operationId: String): PreviewProvisionResult
    fun approvalResult(): String
    fun verifyEntitlement(session: PreviewSession, purchase: PreviewPurchaseResult): String
}

internal class FakePreviewAuthProvider : PreviewAuthProvider {
    private var session: PreviewSession? = null
    override suspend fun signIn(provider: String) = PreviewSession("preview-$provider-account", provider).also { session = it }
    override suspend fun restore() = session
    override fun signOut() { session = null }
}

internal class FakePreviewAccountLookupProvider : PreviewAccountLookupProvider, PreviewFixtureSeeder, PreviewFixtureUpdater {
    private val facts = mutableMapOf<String, String>()
    private var seededFact: String? = null
    private var scenarioDefaultAllowed = true
    private var scenario = "new"
    var lookupCount = 0
        private set

    override fun seed(validatedSnapshot: JSONObject) {
        scenario = validatedSnapshot.getString("scenario")
        seededFact = snapshotFact(validatedSnapshot)
        scenarioDefaultAllowed = isFreshDefault(validatedSnapshot)
    }

    override suspend fun lookup(session: PreviewSession): PreviewAccountResult {
        lookupCount++
        val key = key(session)
        val event = facts[key] ?: if (facts.isEmpty()) {
            (seededFact ?: if (scenarioDefaultAllowed) scenarioFact(scenario) else "account_lookup_failed").also {
                facts[key] = it
                seededFact = null
                scenarioDefaultAllowed = false
            }
        } else {
            "account_new".also { facts[key] = it }
        }
        return PreviewAccountResult(event, session.accountId, session.provider)
    }

    override fun recordReceipt(session: PreviewSession, existing: Boolean, purchaseEvent: String) {
        facts[key(session)] = when (purchaseEvent) {
            "purchase_pending" -> if (existing) "account_existing_pending" else "account_pending"
            "purchase_succeeded" -> if (existing) "account_existing_verification_pending" else "account_verification_pending"
            else -> facts[key(session)] ?: "account_new"
        }
    }
    override fun recordEntitlement(session: PreviewSession, existing: Boolean) {
        facts[key(session)] = if (existing) "account_existing" else "account_incomplete"
    }
    override fun recordEntitlementRejected(session: PreviewSession, existing: Boolean) {
        facts[key(session)] = if (existing) "account_lapsed" else "account_new"
    }
    override fun recordProvisioning(session: PreviewSession, operationId: String) {
        facts[key(session)] = "account_incomplete_provisioning"
    }
    override fun recordProvisioned(session: PreviewSession) {
        facts[key(session)] = "account_existing"
    }

    private fun key(session: PreviewSession) = "${session.provider}:${session.accountId}"
    private fun snapshotFact(snapshot: JSONObject): String? {
        val account = snapshot.getString("account_state")
        val classifiedAccount = when (account) {
            "new_account", "incomplete", "existing" -> account
            "signed_in" -> snapshot.optionalString("resume_account_state")
            else -> null
        } ?: legacyAccountHint(snapshot)
        val entitlement = snapshot.getString("entitlement_state")
        val resumeScreen = snapshot.optionalString("resume_screen")
        fun suffix(base: String) = when (entitlement) {
            "grace" -> "${base}_grace"
            "billing_retry" -> "${base}_billing_retry"
            else -> base
        }
        return when (classifiedAccount) {
            "existing" -> when (entitlement) {
                "store_pending" -> "account_existing_pending"
                "store_succeeded_unverified", "verifying" -> "account_existing_verification_pending"
                "unavailable" -> "account_existing_store_unavailable"
                "none", "expired" -> "account_lapsed"
                "revoked" -> "account_lapsed_revoked"
                "active", "grace", "billing_retry" -> suffix("account_existing")
                else -> null
            }
            "incomplete" -> when (entitlement) {
                "unavailable" -> "account_incomplete_store_unavailable"
                "active", "grace", "billing_retry" -> suffix(if (resumeScreen == "provisioning") "account_incomplete_provisioning" else "account_incomplete")
                else -> null
            }
            "new_account" -> when (entitlement) {
                "store_pending" -> "account_pending"
                "store_succeeded_unverified", "verifying" -> "account_verification_pending"
                "unavailable" -> "account_store_unavailable"
                "none" -> "account_new"
                "active", "grace", "billing_retry" -> suffix(if (resumeScreen == "provisioning") "account_incomplete_provisioning" else "account_incomplete")
                else -> null
            }
            else -> null
        }
    }

    private fun legacyAccountHint(snapshot: JSONObject): String? = when (snapshot.optionalString("resume_screen")) {
        "passphrase", "provisioning" -> "incomplete"
        "join", "unlock" -> "existing"
        else -> null
    }

    private fun isFreshDefault(snapshot: JSONObject) =
        !snapshot.getBoolean("rejected") &&
            snapshot.getString("screen") == "welcome" &&
            snapshot.getString("account_state") == "anonymous" &&
            snapshot.getString("entitlement_state") == "none" &&
            snapshot.optionalString("resume_screen") == null

    private fun JSONObject.optionalString(key: String): String? =
        if (!has(key) || isNull(key)) null else optString(key).ifEmpty { null }

    private fun scenarioFact(value: String) = when (value) {
        "returning", "approval_denied" -> "account_existing"
        "lapsed" -> "account_lapsed"
        "pending" -> "account_pending"
        "store_unavailable" -> "account_store_unavailable"
        "provision_retry" -> "account_incomplete_provisioning"
        else -> "account_new"
    }
}

internal class FakePreviewPurchaseProvider : PreviewPurchaseProvider {
    override fun purchaseResult(session: PreviewSession) = PreviewPurchaseResult("purchase_succeeded", session.accountId, session.provider)
    override fun displayPrice() = "$4.99/month · Preview price"
}
internal class FakePreviewPreparationProvider : PreviewPreparationProvider {
    override suspend fun prepare(session: PreviewSession) = PreviewPreparationResult("sync_finished", session.accountId, session.provider)
}
internal open class FakePreviewHostedService : PreviewHostedService {
    override suspend fun provision(session: PreviewSession, operationId: String) = PreviewProvisionResult("provision_finished", session.accountId, session.provider, operationId)
    override fun approvalResult() = "approval_granted"
    override fun verifyEntitlement(session: PreviewSession, purchase: PreviewPurchaseResult) = if (purchase.matches(session)) "entitlement_verified" else "entitlement_rejected"
}
