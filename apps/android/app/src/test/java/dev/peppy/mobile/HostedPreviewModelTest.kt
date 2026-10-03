package dev.peppy.mobile

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.Job
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class HostedPreviewModelTest {
    private val context get() = ApplicationProvider.getApplicationContext<Context>()
    @Before fun clearCheckpoint() { context.getSharedPreferences("hosted-preview-checkpoint.v1", Context.MODE_PRIVATE).edit().clear().commit() }
    private fun HostedPreviewModel.signInNew() { advance("hosted_start"); signIn("google") }
    private fun HostedPreviewModel.toPassphrase() { signInNew(); purchase(); verifyEntitlement(); assertEquals("passphrase", screen) }
    private fun HostedPreviewModel.toSettings() {
        toPassphrase()
        val phrase = passphrase
        assertTrue(createPassphrase(phrase)); assertTrue(confirmCreation(phrase))
        provision(); advance("permissions_done"); assertEquals("settings", screen)
    }

    @Test fun nullFieldsAndConfirmationSecretsRemainAbsentFromCheckpoint() {
        val model = HostedPreviewModel(context)
        assertNull(model.statusKey); assertNull(model.operationId)
        model.toPassphrase()
        val canary = model.passphrase
        assertTrue(canary.isNotEmpty()); assertFalse(model.snapshot.contains(canary))
        assertTrue(model.createPassphrase(canary)); assertTrue(model.confirmCreation(canary))
        assertEquals("provisioning", model.screen); assertEquals("", model.passphrase)
        assertFalse(model.snapshot.contains(canary))
    }

    @Test fun interruptionAndProcessRestartRequirePhraseReentryWithoutRegeneration() {
        val model = HostedPreviewModel(context); model.toPassphrase(); model.createPassphrase(model.passphrase)
        model.resumeAfterInterruption()
        assertEquals("passphrase", model.screen); assertEquals("", model.passphrase)
        val restored = HostedPreviewModel(context)
        assertEquals("passphrase", restored.screen); assertEquals("", restored.passphrase)
        assertNotNull(restored.accountId)
    }

    @Test fun paidBackAndContinueNeverCallStoreAgain() {
        val store = CountingStore()
        val model = HostedPreviewModel(context, purchase = store)
        model.toPassphrase(); model.advance("back"); model.purchase()
        assertEquals("passphrase", model.screen)
        assertEquals(1, store.purchases); assertEquals(0, store.restores)
        assertNull(model.statusKey)
    }

    @Test fun returningUnlockDoesNotApplyCreationStrengthRules() {
        val model = HostedPreviewModel(context); model.chooseScenario("returning"); model.signInNew()
        model.advance("show_approval"); model.approval()
        assertEquals("unlock", model.screen)
        assertFalse(model.unlock("")); assertTrue(model.unlock("old existing phrase"))
        assertEquals("permissions", model.screen)
    }

    @Test fun pendingApprovalRestoresAndValidatesSeparately() {
        val model = HostedPreviewModel(context); model.chooseScenario("pending"); model.signInNew()
        assertEquals("purchase_pending", model.screen)
        model.restore(); assertEquals("subscription_verifying", model.screen)
        model.verifyEntitlement(); assertEquals("passphrase", model.screen)
    }

    @Test fun interruptedVerificationReconcilesWithoutPurchase() {
        val model = HostedPreviewModel(context); model.signInNew(); model.purchase(); model.advance("verify_entitlement")
        val restored = HostedPreviewModel(context)
        restored.verifyEntitlement()
        assertEquals("passphrase", restored.screen); assertNull(restored.localError)
    }

    @Test fun mismatchedAccountReceiptIsRejectedBeforeEntitlement() {
        val store = object : PreviewPurchaseProvider by FakePreviewPurchaseProvider {
            override fun purchaseResult(accountId: String) = PreviewPurchaseResult("purchase_succeeded", "another-account")
        }
        val model = HostedPreviewModel(context, purchase = store); model.signInNew(); model.purchase()
        assertEquals("subscribe", model.screen); assertEquals("preview_error", model.localError)
        assertEquals("none", model.entitlement)
    }

    @Test fun serverRejectionCannotCreateServerDespiteStoreSuccess() {
        val service = object : PreviewHostedService by FakePreviewHostedService {
            override fun verifyEntitlement(accountId: String, purchase: PreviewPurchaseResult) = "entitlement_rejected"
        }
        val model = HostedPreviewModel(context, hostedService = service); model.signInNew(); model.purchase(); model.verifyEntitlement()
        assertEquals("subscribe", model.screen); assertEquals("", model.passphrase)
    }

    @Test fun unavailableStoreBlocksPurchaseUntilExplicitRetry() {
        val store = CountingStore(); val model = HostedPreviewModel(context, purchase = store)
        model.chooseScenario("store_unavailable"); model.signInNew(); model.purchase()
        assertEquals(0, store.purchases); assertEquals("subscribe", model.screen)
        model.retryStore(); model.purchase(); assertEquals(1, store.purchases)
    }

    @Test fun provisionRetryAndExpiryKeepExistingIdentity() {
        val model = HostedPreviewModel(context); model.chooseScenario("provision_retry"); model.signInNew()
        val operation = model.operationId
        model.provision(); assertEquals("provisioning", model.screen)
        model.advance("provision_retry"); model.provision(); model.advance("permissions_done")
        assertEquals(operation, model.operationId)
        model.advance("entitlement_expired"); assertEquals("settings", model.screen)
        model.advance("resubscribe"); assertEquals("subscribe", model.screen)
        model.restore(); model.verifyEntitlement(); assertEquals("join", model.screen)
    }

    @Test fun signoutDeletionAndScenarioChangesCancelWorkAndClearIdentity() {
        val model = HostedPreviewModel(context); model.toSettings()
        model.advance("delete_account"); model.advance("cancel"); assertEquals("settings", model.screen)
        model.advance("delete_account"); model.advance("deletion_confirmed")
        assertEquals("welcome", model.screen); assertNull(model.accountId)
        model.signInNew(); model.generatePassphrase()
        val job = Job(); model.provisionJob = job; model.chooseScenario("returning")
        assertTrue(job.isCancelled); assertEquals("", model.passphrase); assertNull(model.accountId)
    }

    private class CountingStore : PreviewPurchaseProvider {
        var purchases = 0; var restores = 0
        override fun purchaseResult(accountId: String): PreviewPurchaseResult { purchases++; return PreviewPurchaseResult("purchase_succeeded", accountId) }
        override fun restoreResult(accountId: String): PreviewPurchaseResult { restores++; return PreviewPurchaseResult("restore_succeeded", accountId) }
        override fun displayPrice() = "Preview price"
    }
}
