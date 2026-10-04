package dev.peppy.mobile

import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.LifecycleRegistry
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.peppy.mobile.ui.theme.PeppyTheme
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

/** Exercises injected preview fixtures only, never NativeGateway or real enrollment. */
@RunWith(AndroidJUnit4::class)
class HostedOnboardingUiTest {
    @get:Rule val ui = createComposeRule()

    @Test fun newAccountAutomaticallyResolvesWithoutFixtureControls() {
        val fixture = fixture("account_new")
        setContent(fixture)
        tap("onboarding-hosted-cta"); tap("hosted-signin-google")

        waitFor("preview-purchase")
        assertEquals(1, fixture.lookup.calls)
        ui.onAllNodesWithTag("preview-scenario-menu").assertCountEquals(0)
        ui.onAllNodesWithTag("preview-restore").assertCountEquals(0)
        ui.onAllNodesWithTag("preview-reset").assertCountEquals(0)
    }

    @Test fun returningAccountUnlockAutomaticallyPreparesData() {
        val fixture = fixture("account_existing")
        setContent(fixture)
        tap("onboarding-hosted-cta"); tap("hosted-signin-apple")
        waitFor("preview-show-approval")
        ui.onNodeWithTag("passphrase-fallback-button").assertIsDisplayed()
        tap("preview-show-approval"); ui.onNodeWithTag("device-allow-button").performClick()
        ui.onNodeWithTag("unlock-passphrase").performTextInput("secret")
        tap("unlock-button")

        waitFor("permissions-screen")
        assertEquals(1, fixture.preparation.calls)
        ui.onNodeWithTag("create-server").assertDoesNotExist()
    }

    @Test fun provisioningFailureWaitsForRetryThenCompletes() {
        val hosted = FailsOnceHostedService()
        val fixture = fixture("account_incomplete_provisioning", hosted = hosted)
        setContent(fixture)
        tap("onboarding-hosted-cta"); tap("hosted-signin-google")

        waitFor("provisioning-retry")
        assertEquals(1, hosted.calls)
        tap("provisioning-retry")
        waitFor("permissions-screen")
        assertEquals(2, hosted.calls)
    }

    @Test fun verificationExceptionShowsRetryAndReusesPurchase() {
        val hosted = FailsOnceVerificationService()
        val fixture = fixture("account_new", hosted = hosted)
        setContent(fixture)
        tap("onboarding-hosted-cta"); tap("hosted-signin-google"); waitFor("preview-purchase"); tap("preview-purchase")

        waitFor("subscription-verify-retry")
        tap("subscription-verify-retry")

        waitFor("passphrase-create-screen")
        assertEquals(2, hosted.calls)
    }

    @Test fun lapsedScreenExposesSemanticSignout() {
        val fixture = fixture("account_lapsed")
        setContent(fixture)
        tap("onboarding-hosted-cta"); tap("hosted-signin-apple")
        waitFor("lapsed-signout")
        ui.onNodeWithTag("lapsed-signout").assertIsDisplayed()
    }

    @Test fun stopStartReconcilesPendingExactlyOncePerForeground() {
        val owner = TestLifecycleOwner()
        val fixture = fixture("account_pending", restoreSession = true)
        setContent(fixture, owner)
        tap("onboarding-hosted-cta"); tap("hosted-signin-google")
        waitFor("preview-pending-recheck")
        assertEquals(1, fixture.lookup.calls)
        ui.onNodeWithTag("preview-status").assertDoesNotExist()

        ui.runOnIdle { owner.handle(Lifecycle.Event.ON_STOP) }
        ui.runOnIdle { owner.handle(Lifecycle.Event.ON_START) }

        waitUntil { fixture.lookup.calls == 2 }
        ui.onNodeWithTag("preview-pending-recheck").assertIsDisplayed()
        assertEquals(2, fixture.lookup.calls)
        ui.waitForIdle()
        assertEquals(2, fixture.lookup.calls)
    }

    private fun setContent(fixture: Fixture, owner: TestLifecycleOwner = TestLifecycleOwner()) {
        ui.setContent {
            CompositionLocalProvider(LocalLifecycleOwner provides owner) {
                PeppyTheme { HostedOnboardingScreen(onSelfHosted = {}, injectedModel = fixture.model) }
            }
        }
        ui.runOnIdle {
            owner.handle(Lifecycle.Event.ON_CREATE)
            owner.handle(Lifecycle.Event.ON_START)
        }
    }

    private fun fixture(event: String, hosted: PreviewHostedService = FakePreviewHostedService(), restoreSession: Boolean = false): Fixture {
        val auth = UiAuth(restoreSession)
        val lookup = UiLookup(event)
        val preparation = UiPreparation()
        val model = HostedPreviewModel(
            ApplicationProvider.getApplicationContext(),
            auth = auth,
            accountLookup = lookup,
            preparation = preparation,
            hostedService = hosted,
            storage = MemoryStorage(),
        )
        return Fixture(model, lookup, preparation)
    }

    private data class Fixture(val model: HostedPreviewModel, val lookup: UiLookup, val preparation: UiPreparation)
    private class MemoryStorage : PreviewCheckpointStorage {
        private var value: String? = null
        override fun v2() = value
        override fun v1(): String? = null
        override fun saveV2(snapshot: String) { value = snapshot }
    }
    private class UiAuth(private val restoreSession: Boolean) : PreviewAuthProvider {
        private var session: PreviewSession? = null
        override suspend fun signIn(provider: String) = PreviewSession("ui-$provider", provider).also { session = it }
        override suspend fun restore() = if (restoreSession) session else null
        override fun signOut() { session = null }
    }
    private class UiLookup(private val event: String) : PreviewAccountLookupProvider {
        var calls = 0
        override suspend fun lookup(session: PreviewSession) = PreviewAccountResult(event, session.accountId, session.provider).also { calls++ }
    }
    private class UiPreparation : PreviewPreparationProvider {
        var calls = 0
        override suspend fun prepare(session: PreviewSession) = PreviewPreparationResult("sync_finished", session.accountId, session.provider).also { calls++ }
    }
    private class FailsOnceVerificationService : FakePreviewHostedService() {
        var calls = 0
        override fun verifyEntitlement(session: PreviewSession, purchase: PreviewPurchaseResult): String {
            calls++
            if (calls == 1) error("verification unavailable")
            return "entitlement_verified"
        }
    }
    private class FailsOnceHostedService : FakePreviewHostedService() {
        var calls = 0
        override suspend fun provision(session: PreviewSession, operationId: String): PreviewProvisionResult {
            calls++
            return PreviewProvisionResult(if (calls == 1) "provision_failed" else "provision_finished", session.accountId, session.provider, operationId)
        }
    }
    private class TestLifecycleOwner : LifecycleOwner {
        private val registry = LifecycleRegistry(this)
        override val lifecycle: Lifecycle get() = registry
        fun handle(event: Lifecycle.Event) { registry.handleLifecycleEvent(event) }
    }

    private fun tap(tag: String) = ui.onNodeWithTag(tag).performScrollTo().performClick()
    private fun waitFor(tag: String) = waitUntil { ui.onAllNodesWithTag(tag).fetchSemanticsNodes().isNotEmpty() }
    private fun waitUntil(condition: () -> Boolean) = ui.waitUntil(10_000, condition)
}
