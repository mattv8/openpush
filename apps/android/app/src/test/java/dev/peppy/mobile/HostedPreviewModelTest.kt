package dev.peppy.mobile

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class HostedPreviewModelTest {
    private val context get() = ApplicationProvider.getApplicationContext<Context>()

    @Test fun googleAndAppleColdPaidRestoreBindFactsToAuthenticatedIdentity() = runBlocking {
        for (provider in listOf("google", "apple")) {
            for ((checkpoint, expectedScreen) in listOf(paidPassphraseCheckpoint() to "passphrase", provisionedCheckpoint() to "join")) {
                val storage = MemoryStorage(v2Value = checkpoint)
                val lookup = FakePreviewAccountLookupProvider()
                val model = model(auth = RestoringAuth("$provider-id", provider), lookup = lookup, storage = storage)

                model.onForeground()

                assertEquals(provider, model.signInProvider)
                assertEquals(expectedScreen, model.screen)
                assertEquals(1, lookup.lookupCount)
            }
        }
    }

    @Test fun newAndExistingPendingFactsRemainDistinctAcrossForegroundRecheck() = runBlocking {
        for ((event, expectedAccount) in listOf("account_pending" to "new_account", "account_existing_pending" to "existing")) {
            val lookup = QueueLookup(event, event)
            val model = model(auth = RestoringAuth("id", "google"), lookup = lookup)
            model.advance("hosted_start")
            model.signIn("google")
            assertEquals("purchase_pending", model.screen)

            model.onBackground()
            model.onForeground()

            assertEquals("purchase_pending", model.screen)
            assertEquals(expectedAccount, JSONObject(model.snapshot).getString("account_state"))
            assertEquals(2, lookup.calls)
        }
    }

    @Test fun coldInterruptedUnlockRequiresReapprovalThenPreparesOnce() = runBlocking {
        val preparation = QueuePreparation("sync_finished")
        val model = model(
            auth = RestoringAuth("returning-id", "apple"),
            lookup = FakePreviewAccountLookupProvider(),
            preparation = preparation,
            storage = MemoryStorage(v2Value = syncingCheckpoint()),
        )

        model.onForeground()
        assertEquals("join", model.screen)
        model.advance("approval_granted"); assertTrue(model.unlock("secret")); model.prepare()

        assertEquals("permissions", model.screen)
        assertEquals(1, preparation.calls)
    }

    @Test fun interruptedVerificationRecoversReceiptWithoutAnotherPurchase() = runBlocking {
        val purchase = CountingPurchase()
        val firstLookup = FakePreviewAccountLookupProvider()
        val auth = RestoringAuth("apple-id", "apple")
        val storage = MemoryStorage()
        val first = model(auth = auth, lookup = firstLookup, purchase = purchase, storage = storage)
        first.advance("hosted_start"); first.signIn("apple"); first.purchase()
        assertEquals("subscription_verifying", first.screen)
        assertEquals(1, purchase.calls)
        first.onBackground()

        val restarted = model(auth = auth, lookup = FakePreviewAccountLookupProvider(), purchase = purchase, storage = storage)
        restarted.onForeground()
        assertEquals("subscription_verifying", restarted.screen)
        restarted.verifyEntitlement()

        assertEquals("passphrase", restarted.screen)
        assertEquals(1, purchase.calls)
    }

    @Test fun renewalReceiptRecoversAsExistingAndRoutesToJoin() = runBlocking {
        val storage = MemoryStorage(v2Value = existingVerificationCheckpoint())
        val model = model(auth = RestoringAuth("google-id", "google"), lookup = FakePreviewAccountLookupProvider(), storage = storage)

        model.onForeground()
        assertEquals("subscription_verifying", model.screen)
        model.verifyEntitlement()

        assertEquals("join", model.screen)
    }

    @Test fun staleNonCooperativeAuthCannotWriteSessionOrClearNewBusyWork() = runBlocking {
        val auth = DeferredAuth()
        val lookup = DeferredLookup()
        val model = model(auth = auth, lookup = lookup)
        model.advance("hosted_start")
        val stale = async(Dispatchers.Default) { model.signIn("google") }
        auth.started.await()
        model.onBackground()
        auth.complete(PreviewSession("stale", "google"))
        stale.await()

        assertNull(model.accountId)
        assertEquals("signin", model.screen)
    }

    @Test fun staleLookupAndPreparationAndProvisionResultsAreIgnored() = runBlocking {
        val lookup = DeferredLookup()
        val model = model(auth = StaticAuth("id", "google"), lookup = lookup)
        model.advance("hosted_start")
        val signing = async(Dispatchers.Default) { model.signIn("google") }
        lookup.started.await(); model.onBackground()
        lookup.complete(PreviewAccountResult("account_new", "id", "google")); signing.await()
        assertEquals("signin", model.screen)

        val prep = DeferredPreparation()
        val syncing = modelAtSyncing(prep)
        val preparing = async(Dispatchers.Default) { syncing.prepare() }
        prep.started.await(); syncing.onBackground()
        prep.complete(PreviewPreparationResult("sync_finished", "returning-id", "google")); preparing.await()
        assertEquals("signin", syncing.screen)

        val provision = DeferredHostedService()
        val provisioning = modelAtProvisioning(provision)
        val running = async(Dispatchers.Default) { provisioning.provision() }
        provision.started.await(); provisioning.onBackground()
        provision.complete(PreviewProvisionResult("provision_finished", "new-id", "google", "preview-provision-new")); running.await()
        assertEquals("signin", provisioning.screen)
    }

    @Test fun preparationFailureWaitsForManualRetry() = runBlocking {
        val preparation = QueuePreparation("sync_failed", "sync_finished")
        val model = modelAtSyncing(preparation)

        model.prepare()
        assertEquals("hosted_data_prepare_failed", model.statusKey)
        assertEquals(1, preparation.calls)
        model.prepare()
        assertEquals(1, preparation.calls)
        model.retryPreparation(); model.prepare()

        assertEquals("permissions", model.screen)
        assertEquals(2, preparation.calls)
    }

    @Test fun providerMismatchIsRejectedAndCancellationIsSilent() = runBlocking {
        val mismatch = model(auth = StaticAuth("id", "apple"), lookup = QueueLookup("account_new"))
        mismatch.advance("hosted_start"); mismatch.signIn("google")
        assertEquals("signin", mismatch.screen)
        assertEquals("preview_identity_mismatch", mismatch.localError)

        val cancelled = model(auth = StaticAuth("id", "google"), lookup = CancellingLookup())
        cancelled.advance("hosted_start")
        try { cancelled.signIn("google"); fail("expected cancellation") } catch (_: kotlinx.coroutines.CancellationException) {}
        assertEquals("checking_account", cancelled.screen)
        assertNull(cancelled.statusKey)
    }

    @Test fun storeRetryStartsOneLookupAndBusySuppressesDuplicateRetry() = runBlocking {
        val lookup = DeferredLookup(first = PreviewAccountResult("account_store_unavailable", "id", "google"))
        val model = model(auth = StaticAuth("id", "google"), lookup = lookup)
        model.advance("hosted_start"); model.signIn("google")
        assertEquals("subscribe", model.screen)

        val retry = async(Dispatchers.Default) { model.retryStore() }
        lookup.started.await()
        model.retryAccount()
        assertEquals(2, lookup.calls)
        lookup.complete(PreviewAccountResult("account_new", "id", "google")); retry.await()
        assertEquals("subscribe", model.screen)
    }

    @Test fun completedSignupSignsBackIntoExistingAccountWithoutRepurchase() = runBlocking {
        val lookup = FakePreviewAccountLookupProvider()
        val purchase = CountingPurchase()
        val model = model(auth = StaticAuth("id", "google"), lookup = lookup, purchase = purchase)
        model.advance("hosted_start"); model.signIn("google"); model.purchase(); model.verifyEntitlement()
        model.generatePassphrase(); val phrase = model.passphrase
        assertTrue(model.createPassphrase(phrase)); assertTrue(model.confirmCreation(phrase)); model.provision()
        model.advance("permissions_done"); model.advance("signout"); model.advance("hosted_start"); model.signIn("google")

        assertEquals("join", model.screen)
        assertEquals(1, purchase.calls)
    }

    @Test fun mismatchedLookupAndPurchaseProviderNeverAdvanceTrustedRoutes() = runBlocking {
        val lookupMismatch = model(
            auth = StaticAuth("id", "google"),
            lookup = object : PreviewAccountLookupProvider {
                override suspend fun lookup(session: PreviewSession) = PreviewAccountResult("account_existing", session.accountId, "apple")
            },
        )
        lookupMismatch.advance("hosted_start"); lookupMismatch.signIn("google")
        assertEquals("checking_account", lookupMismatch.screen)
        assertEquals("hosted_account_check_failed", lookupMismatch.statusKey)
        assertEquals("preview_identity_mismatch", lookupMismatch.localError)

        val purchaseMismatch = model(
            auth = StaticAuth("id", "google"),
            lookup = QueueLookup("account_new"),
            purchase = object : PreviewPurchaseProvider {
                override fun purchaseResult(session: PreviewSession) = PreviewPurchaseResult("purchase_succeeded", session.accountId, "apple")
                override fun displayPrice() = "$4.99/month"
            },
        )
        purchaseMismatch.advance("hosted_start"); purchaseMismatch.signIn("google"); purchaseMismatch.purchase()
        assertEquals("subscribe", purchaseMismatch.screen)
        assertEquals("preview_identity_mismatch", purchaseMismatch.localError)
    }

    @Test fun rejectedEntitlementDoesNotRecoverInvalidReceiptOnForeground() = runBlocking {
        val lookup = FakePreviewAccountLookupProvider()
        val auth = RestoringAuth("id", "google")
        val hosted = object : FakePreviewHostedService() {
            override fun verifyEntitlement(session: PreviewSession, purchase: PreviewPurchaseResult) = "entitlement_rejected"
        }
        val model = model(auth = auth, lookup = lookup, hosted = hosted)
        model.advance("hosted_start"); model.signIn("google"); model.purchase(); model.verifyEntitlement()
        assertEquals("subscribe", model.screen)

        model.onBackground(); model.onForeground()

        assertEquals("subscribe", model.screen)
        assertEquals("none", model.entitlement)
    }

    @Test fun coldCheckingAndLookupErrorCheckpointsPreserveClassificationWithoutRepurchase() = runBlocking {
        data class Case(
            val classification: String,
            val entitlement: String,
            val resumeScreen: String?,
            val status: String?,
            val expectedScreen: String,
            val expectedAccount: String,
        )
        val cases = listOf(
            Case("incomplete", "active", "passphrase", null, "passphrase", "incomplete"),
            Case("incomplete", "active", "provisioning", "hosted_account_check_failed", "provisioning", "incomplete"),
            Case("existing", "store_pending", "purchase_pending", "hosted_account_check_failed", "purchase_pending", "existing"),
            Case("existing", "unavailable", "subscribe", "hosted_account_check_failed", "subscribe", "existing"),
            Case("new_account", "unavailable", "subscribe", null, "subscribe", "new_account"),
            Case("existing", "store_succeeded_unverified", "subscription_verifying", null, "subscription_verifying", "existing"),
        )
        for (case in cases) {
            val purchase = CountingPurchase()
            val model = model(
                auth = RestoringAuth("cold-${case.classification}-${case.entitlement}", "apple"),
                lookup = FakePreviewAccountLookupProvider(),
                purchase = purchase,
                storage = MemoryStorage(v2Value = checkingCheckpoint(case.classification, case.entitlement, case.resumeScreen, case.status)),
            )

            model.onForeground()

            assertEquals(case.expectedScreen, model.screen)
            assertEquals(case.expectedAccount, model.accountState)
            assertEquals(0, purchase.calls)
            if (case.entitlement == "store_succeeded_unverified") {
                model.verifyEntitlement()
                assertEquals("join", model.screen)
                assertEquals(0, purchase.calls)
            }
        }
    }

    @Test fun coldAmbiguousCheckingFactsStayAtLookupErrorWithoutPurchase() = runBlocking {
        for ((entitlement, resumeScreen) in listOf(
            "active" to null,
            "store_pending" to "purchase_pending",
            "unavailable" to "subscribe",
        )) {
            val purchase = CountingPurchase()
            val model = model(
                auth = RestoringAuth("unknown-$entitlement", "google"),
                lookup = FakePreviewAccountLookupProvider(),
                purchase = purchase,
                storage = MemoryStorage(v2Value = checkingCheckpoint(null, entitlement, resumeScreen, "hosted_account_check_failed")),
            )

            model.onForeground()

            assertEquals("checking_account", model.screen)
            assertEquals("hosted_account_check_failed", model.statusKey)
            assertEquals(0, purchase.calls)
        }
    }

    @Test fun checkingCheckpointSeedsClassifiedFactsFromResumeAccountState() = runBlocking {
        val cases = listOf(
            Triple("incomplete", "active", "account_incomplete"),
            Triple("existing", "store_pending", "account_existing_pending"),
            Triple("existing", "verifying", "account_existing_verification_pending"),
            Triple("existing", "unavailable", "account_existing_store_unavailable"),
            Triple("new_account", "unavailable", "account_store_unavailable"),
            Triple("existing", "none", "account_lapsed"),
        )
        for ((classification, entitlement, expected) in cases) {
            val lookup = FakePreviewAccountLookupProvider()
            lookup.seed(checkingSeed(classification, entitlement))

            val result = lookup.lookup(PreviewSession("id-$classification-$entitlement", "apple"))

            assertEquals(expected, result.event)
        }
        val provisioning = FakePreviewAccountLookupProvider()
        provisioning.seed(checkingSeed("incomplete", "active", "provisioning"))
        assertEquals("account_incomplete_provisioning", provisioning.lookup(PreviewSession("provision-id", "google")).event)
    }

    @Test fun ambiguousCheckingCheckpointFailsLookupInsteadOfGuessingNew() = runBlocking {
        for (entitlement in listOf("active", "store_pending", "verifying", "unavailable")) {
            val lookup = FakePreviewAccountLookupProvider()
            lookup.seed(checkingSeed(null, entitlement))

            assertEquals("account_lookup_failed", lookup.lookup(PreviewSession("id-$entitlement", "google")).event)
        }
    }

    @Test fun legacyUnambiguousResumeScreensStillSeedClassification() = runBlocking {
        for ((screen, expected) in listOf(
            "passphrase" to "account_incomplete",
            "provisioning" to "account_incomplete_provisioning",
            "join" to "account_existing",
            "unlock" to "account_existing",
        )) {
            val lookup = FakePreviewAccountLookupProvider()
            lookup.seed(checkingSeed(null, "active", screen, includeClassificationField = false))

            assertEquals(expected, lookup.lookup(PreviewSession("legacy-$screen", "apple")).event)
        }
    }

    @Test fun explicitSessionEndingClearsAuthWithoutBackgroundSignout() = runBlocking {
        val auth = FakePreviewAuthProvider()
        val model = model(auth = auth, lookup = QueueLookup("account_new", "account_new", "account_new"))
        model.advance("hosted_start"); model.signIn("google")
        assertNotNull(auth.restore())

        model.onBackground()
        assertNotNull(auth.restore())
        model.onForeground()
        model.advance("signout")
        assertNull(auth.restore())
        model.advance("hosted_start"); model.onBackground(); model.onForeground()
        assertEquals("signin", model.screen)
        assertNull(model.accountId)

        model.signIn("google"); model.advance("reset")
        assertNull(auth.restore())
        model.advance("hosted_start"); model.signIn("google"); model.advance("deletion_confirmed")
        assertNull(auth.restore())
    }

    @Test fun verificationFailuresAndMissingReceiptExposeRetryWithoutRepurchase() = runBlocking {
        val purchase = CountingPurchase()
        val hosted = ThrowsOnceVerificationService()
        val model = model(auth = StaticAuth("id", "google"), lookup = QueueLookup("account_new"), purchase = purchase, hosted = hosted)
        model.advance("hosted_start"); model.signIn("google"); model.purchase()

        model.verifyEntitlement()
        assertEquals("subscription_verifying", model.screen)
        assertEquals("preview_error", model.localError)
        model.retryVerification()

        assertEquals("passphrase", model.screen)
        assertEquals(2, hosted.verificationCalls)
        assertEquals(1, purchase.calls)

        val missing = model(auth = StaticAuth("missing", "google"), lookup = QueueLookup("account_new"))
        missing.advance("hosted_start"); missing.signIn("google"); missing.advance("purchase_succeeded")
        missing.verifyEntitlement()
        assertEquals("subscription_verifying", missing.screen)
        assertEquals("preview_error", missing.localError)
    }

    @Test fun purchaseExceptionAndProviderMismatchesBecomeRecoverableFailures() = runBlocking {
        val purchaseFailure = model(
            auth = StaticAuth("purchase", "google"),
            lookup = QueueLookup("account_new"),
            purchase = object : PreviewPurchaseProvider {
                override fun purchaseResult(session: PreviewSession): PreviewPurchaseResult = error("store unavailable")
                override fun displayPrice() = "$4.99/month"
            },
        )
        purchaseFailure.advance("hosted_start"); purchaseFailure.signIn("google"); purchaseFailure.purchase()
        assertEquals("subscribe", purchaseFailure.screen)
        assertEquals("preview_error", purchaseFailure.localError)

        val preparationMismatch = modelAtSyncing(object : PreviewPreparationProvider {
            override suspend fun prepare(session: PreviewSession) = PreviewPreparationResult("sync_finished", "other", session.provider)
        })
        preparationMismatch.prepare()
        assertEquals("hosted_data_prepare_failed", preparationMismatch.statusKey)
        preparationMismatch.retryPreparation()
        assertNull(preparationMismatch.statusKey)

        val provisionMismatch = modelAtProvisioning(object : PreviewHostedService by FakePreviewHostedService() {
            override suspend fun provision(session: PreviewSession, operationId: String) =
                PreviewProvisionResult("provision_finished", session.accountId, "apple", operationId)
        })
        provisionMismatch.provision()
        assertEquals("provisioning_error", provisionMismatch.statusKey)
        provisionMismatch.advance("provision_retry")
        assertNull(provisionMismatch.statusKey)
    }

    @Test fun strictV1MigrationWritesV2AndLeavesOldValueUntouched() {
        val v1 = uniffi.peppy_mobile_bindings.hostedPreviewAdvance(
            uniffi.peppy_mobile_bindings.hostedPreviewAdvance(uniffi.peppy_mobile_bindings.hostedPreviewStart("provision_retry"), "hosted_start"),
            "signed_in",
        )
        val storage = MemoryStorage(v1Value = v1)
        val model = model(storage = storage)

        assertEquals(2, JSONObject(model.snapshot).getInt("version"))
        assertEquals(v1, storage.v1())
        assertEquals(model.snapshot, storage.v2())
    }

    @Test fun malformedV2TakesPrecedenceOverV1AndFailsSafe() {
        val storage = MemoryStorage(v2Value = "not-json", v1Value = uniffi.peppy_mobile_bindings.hostedPreviewStart("returning"))
        val model = model(storage = storage)

        assertEquals("welcome", model.screen)
        assertTrue(JSONObject(model.snapshot).getBoolean("rejected"))
    }

    @Test fun confirmationRecordsOperationAndColdRestartResumesSameProvisioning() = runBlocking {
        val storage = MemoryStorage(v2Value = paidPassphraseCheckpoint())
        val lookup = FakePreviewAccountLookupProvider()
        val auth = RestoringAuth("id", "google")
        val first = model(auth = auth, lookup = lookup, storage = storage)
        first.onForeground(); first.generatePassphrase(); val phrase = first.passphrase; assertTrue(first.createPassphrase(phrase)); assertTrue(first.confirmCreation(phrase))
        val operation = first.operationId
        first.onBackground()

        val restarted = model(auth = auth, lookup = FakePreviewAccountLookupProvider(), storage = storage)
        restarted.onForeground()

        assertEquals("provisioning", restarted.screen)
        assertEquals(operation, restarted.operationId)
        assertFalse(restarted.snapshot.contains(phrase))
    }

    private fun model(
        auth: PreviewAuthProvider = StaticAuth("id", "google"),
        lookup: PreviewAccountLookupProvider = FakePreviewAccountLookupProvider(),
        purchase: PreviewPurchaseProvider = CountingPurchase(),
        preparation: PreviewPreparationProvider = FakePreviewPreparationProvider(),
        hosted: PreviewHostedService = FakePreviewHostedService(),
        storage: MemoryStorage = MemoryStorage(),
        scenario: String = "new",
    ) = HostedPreviewModel(context, auth, purchase, lookup, preparation, hosted, storage, scenario)

    private suspend fun modelAtSyncing(preparation: PreviewPreparationProvider): HostedPreviewModel {
        val model = model(auth = StaticAuth("returning-id", "google"), lookup = QueueLookup("account_existing"), preparation = preparation)
        model.advance("hosted_start"); model.signIn("google"); model.advance("approval_granted"); assertTrue(model.unlock("secret"))
        return model
    }

    private suspend fun modelAtProvisioning(hosted: PreviewHostedService): HostedPreviewModel {
        val model = model(auth = StaticAuth("new-id", "google"), lookup = QueueLookup("account_incomplete"), hosted = hosted)
        model.advance("hosted_start"); model.signIn("google")
        model.generatePassphrase(); val phrase = model.passphrase; assertTrue(model.createPassphrase(phrase)); assertTrue(model.confirmCreation(phrase))
        return model
    }

    private fun paidPassphraseCheckpoint(): String {
        var s = uniffi.peppy_mobile_bindings.hostedPreviewV2Start("new")
        for (e in listOf("hosted_start", "signed_in", "account_incomplete")) s = uniffi.peppy_mobile_bindings.hostedPreviewV2Advance(s, e)
        return s
    }

    private fun checkingCheckpoint(
        classification: String?,
        entitlement: String,
        resumeScreen: String?,
        status: String?,
    ): String = JSONObject(uniffi.peppy_mobile_bindings.hostedPreviewV2Start("new")).apply {
        put("screen", "checking_account")
        put("account_state", "signed_in")
        put("entitlement_state", entitlement)
        put("resume_screen", resumeScreen ?: JSONObject.NULL)
        put("resume_account_state", classification ?: JSONObject.NULL)
        put("status_key", status ?: JSONObject.NULL)
        if (resumeScreen == "provisioning") put("operation_id", "preview-provision-new")
    }.toString()

    private fun checkingSeed(
        classification: String?,
        entitlement: String,
        resumeScreen: String? = null,
        includeClassificationField: Boolean = true,
    ): JSONObject = JSONObject(uniffi.peppy_mobile_bindings.hostedPreviewV2Start("new")).apply {
        put("screen", "checking_account")
        put("account_state", "signed_in")
        put("entitlement_state", entitlement)
        put("resume_screen", resumeScreen ?: JSONObject.NULL)
        if (includeClassificationField) put("resume_account_state", classification ?: JSONObject.NULL)
    }

    private fun provisionedCheckpoint(): String {
        var s = uniffi.peppy_mobile_bindings.hostedPreviewV2Start("new")
        for (e in listOf("hosted_start", "signed_in", "account_incomplete", "local_passphrase_accepted", "passphrase_confirmed", "provision_finished")) s = uniffi.peppy_mobile_bindings.hostedPreviewV2Advance(s, e)
        return s
    }

    private fun syncingCheckpoint(): String {
        var s = uniffi.peppy_mobile_bindings.hostedPreviewV2Start("returning")
        for (e in listOf("hosted_start", "signed_in", "account_existing", "approval_granted", "unlock_succeeded")) s = uniffi.peppy_mobile_bindings.hostedPreviewV2Advance(s, e)
        return s
    }

    private fun existingVerificationCheckpoint(): String {
        var s = uniffi.peppy_mobile_bindings.hostedPreviewV2Start("lapsed")
        for (e in listOf("hosted_start", "signed_in", "account_lapsed", "resubscribe", "purchase_succeeded")) s = uniffi.peppy_mobile_bindings.hostedPreviewV2Advance(s, e)
        return s
    }

    private class MemoryStorage(private var v2Value: String? = null, private val v1Value: String? = null) : PreviewCheckpointStorage {
        override fun v2() = v2Value
        override fun v1() = v1Value
        override fun saveV2(snapshot: String) { v2Value = snapshot }
    }
    private class StaticAuth(private val id: String, private val actualProvider: String) : PreviewAuthProvider {
        override suspend fun signIn(provider: String) = PreviewSession(id, actualProvider)
        override suspend fun restore() = null
        override fun signOut() {}
    }
    private class RestoringAuth(private val id: String, private val provider: String) : PreviewAuthProvider {
        override suspend fun signIn(provider: String) = PreviewSession(id, this.provider)
        override suspend fun restore() = PreviewSession(id, provider)
        override fun signOut() {}
    }
    private class DeferredAuth : PreviewAuthProvider {
        val started = java.util.concurrent.CountDownLatch(1)
        private val release = java.util.concurrent.CountDownLatch(1)
        @Volatile private lateinit var result: PreviewSession
        override suspend fun signIn(provider: String): PreviewSession { started.countDown(); release.await(); return result }
        override suspend fun restore() = null
        override fun signOut() {}
        fun complete(value: PreviewSession) { result = value; release.countDown() }
    }
    private class QueueLookup(vararg events: String) : PreviewAccountLookupProvider {
        private val events = ArrayDeque(events.toList()); var calls = 0
        override suspend fun lookup(session: PreviewSession): PreviewAccountResult { calls++; return PreviewAccountResult(events.removeFirstOrNull() ?: events.lastOrNull() ?: "account_new", session.accountId, session.provider) }
    }
    private class DeferredLookup(private val first: PreviewAccountResult? = null) : PreviewAccountLookupProvider {
        val started = java.util.concurrent.CountDownLatch(1)
        private val release = java.util.concurrent.CountDownLatch(1)
        @Volatile private lateinit var result: PreviewAccountResult
        var calls = 0
        override suspend fun lookup(session: PreviewSession): PreviewAccountResult {
            calls++; if (calls == 1 && first != null) return first
            started.countDown(); release.await(); return result
        }
        fun complete(value: PreviewAccountResult) { result = value; release.countDown() }
    }
    private class CancellingLookup : PreviewAccountLookupProvider { override suspend fun lookup(session: PreviewSession): PreviewAccountResult { throw kotlinx.coroutines.CancellationException("cancel") } }
    private class CountingPurchase : PreviewPurchaseProvider {
        var calls = 0
        override fun purchaseResult(session: PreviewSession) = PreviewPurchaseResult("purchase_succeeded", session.accountId, session.provider).also { calls++ }
        override fun displayPrice() = "$4.99/month"
    }
    private class QueuePreparation(vararg private val events: String) : PreviewPreparationProvider {
        var calls = 0
        override suspend fun prepare(session: PreviewSession) = PreviewPreparationResult(events[calls++], session.accountId, session.provider)
    }
    private class DeferredPreparation : PreviewPreparationProvider {
        val started = java.util.concurrent.CountDownLatch(1)
        private val release = java.util.concurrent.CountDownLatch(1)
        @Volatile private lateinit var result: PreviewPreparationResult
        override suspend fun prepare(session: PreviewSession): PreviewPreparationResult { started.countDown(); release.await(); return result }
        fun complete(value: PreviewPreparationResult) { result = value; release.countDown() }
    }
    private class ThrowsOnceVerificationService : FakePreviewHostedService() {
        var verificationCalls = 0
        override fun verifyEntitlement(session: PreviewSession, purchase: PreviewPurchaseResult): String {
            verificationCalls++
            if (verificationCalls == 1) error("verification unavailable")
            return "entitlement_verified"
        }
    }
    private class DeferredHostedService : PreviewHostedService by FakePreviewHostedService() {
        val started = java.util.concurrent.CountDownLatch(1)
        private val release = java.util.concurrent.CountDownLatch(1)
        @Volatile private lateinit var result: PreviewProvisionResult
        override suspend fun provision(session: PreviewSession, operationId: String): PreviewProvisionResult { started.countDown(); release.await(); return result }
        fun complete(value: PreviewProvisionResult) { result = value; release.countDown() }
    }
}
