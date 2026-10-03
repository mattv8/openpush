package dev.peppy.mobile

import android.content.Context
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.peppy.mobile.ui.theme.PeppyTheme
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

/** Exercises only the isolated preview, never NativeGateway or a real enrollment. */
@RunWith(AndroidJUnit4::class)
class HostedOnboardingUiTest {
    @get:Rule val ui = createComposeRule()

    @Before fun resetPreview() {
        ApplicationProvider.getApplicationContext<Context>()
            .getSharedPreferences("hosted-preview-checkpoint.v1", Context.MODE_PRIVATE).edit().clear().commit()
    }

    private fun tap(id: String) { ui.onNodeWithTag(id).performScrollTo().performClick() }
    private fun waitFor(id: String) {
        ui.waitUntil(10_000) { ui.onAllNodesWithTag(id).fetchSemanticsNodes().isNotEmpty() }
    }

    @Test fun newAccountAndReturningApprovalReachSettings() {
        ui.setContent { PeppyTheme { HostedOnboardingScreen(onSelfHosted = {}) } }
        tap("onboarding-hosted-cta"); tap("hosted-signin-google"); tap("preview-purchase")
        waitFor("generated-passphrase")
        val phrase = ui.onNodeWithTag("generated-passphrase").fetchSemanticsNode().config[SemanticsProperties.EditableText].text
        tap("passphrase-acknowledgement"); tap("create-server")
        ui.onNodeWithTag("passphrase-confirmation").performScrollTo().performTextInput(phrase)
        tap("passphrase-confirm-submit")
        waitFor("permissions-done"); tap("permissions-done")
        waitFor("preview-signout")
        tap("preview-signout"); tap("onboarding-hosted-cta"); tap("hosted-signin-google")
        waitFor("preview-show-approval"); tap("preview-show-approval")
        ui.onNodeWithTag("device-allow-button").performClick()
        ui.onNodeWithTag("unlock-passphrase").performScrollTo().performTextInput("existing phrase")
        tap("unlock-button"); waitFor("permissions-done"); tap("permissions-done")
        ui.onNodeWithTag("manage-subscription").performScrollTo().assertIsDisplayed()
    }

    @Test fun provisioningRetryDoesNotLeaveAStuckSpinner() {
        ui.setContent { PeppyTheme { HostedOnboardingScreen(onSelfHosted = {}) } }
        tap("preview-scenario-menu")
        ui.onNodeWithTag("preview-scenario-provision_retry").performClick()
        tap("onboarding-hosted-cta"); tap("hosted-signin-apple")
        waitFor("provisioning-retry"); tap("provisioning-retry")
        waitFor("permissions-done")
    }
}
