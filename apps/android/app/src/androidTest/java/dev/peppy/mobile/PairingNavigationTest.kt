package dev.peppy.mobile

import android.Manifest
import android.content.pm.PackageManager
import androidx.activity.ComponentActivity
import androidx.compose.material3.Text
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.core.content.ContextCompat
import org.junit.After
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class PairingNavigationTest {
    @get:Rule
    val composeRule = createAndroidComposeRule<ComponentActivity>()

    @After
    fun clearEphemeralPairingState() {
        PairingHost.cancel(InstrumentationRegistry.getInstrumentation().targetContext)
    }

    @Test
    fun cancelReturnsToCallerWithoutRequestingCameraOrRetainingPairingState() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        context.getSharedPreferences("peppy-pairing", 0).edit().putString("test-only", "ephemeral").commit()
        val cameraPermission = ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA)
        showPairingScreen()

        composeRule.onNodeWithTag("qr-pairing-screen").assertIsDisplayed()
        composeRule.onNodeWithTag("qr-cancel-button").assertIsDisplayed()
        assertTrue(ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) == cameraPermission)
        if (cameraPermission == PackageManager.PERMISSION_GRANTED) {
            composeRule.onNodeWithTag("qr-viewfinder").assertIsDisplayed()
        } else {
            composeRule.onNodeWithTag("qr-camera-permission").assertIsDisplayed()
        }

        composeRule.onNodeWithTag("qr-cancel-button").performClick()

        composeRule.onNodeWithTag("pairing-return-screen").assertIsDisplayed()
        composeRule.onAllNodesWithTag("qr-pairing-screen").assertCountEquals(0)
        assertTrue(context.getSharedPreferences("peppy-pairing", 0).all.isEmpty())
    }

    @Test
    fun systemBackReturnsToCallerAndClearsPairingState() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        context.getSharedPreferences("peppy-pairing", 0).edit().putString("test-only", "ephemeral").commit()
        showPairingScreen()

        composeRule.onNodeWithTag("qr-pairing-screen").assertIsDisplayed()
        composeRule.activity.runOnUiThread {
            composeRule.activity.onBackPressedDispatcher.onBackPressed()
        }

        composeRule.onNodeWithTag("pairing-return-screen").assertIsDisplayed()
        composeRule.onAllNodesWithTag("qr-pairing-screen").assertCountEquals(0)
        assertTrue(context.getSharedPreferences("peppy-pairing", 0).all.isEmpty())
    }

    private fun showPairingScreen() {
        composeRule.setContent {
            var pairingOpen by remember { mutableStateOf(true) }
            if (pairingOpen) {
                PairingScreen(onDismiss = { pairingOpen = false }, onFinished = { pairingOpen = false })
            } else {
                Text("Returned", Modifier.testTag("pairing-return-screen"))
            }

        }
    }
}
