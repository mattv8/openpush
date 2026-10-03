package dev.peppy.mobile

import org.junit.Assert.assertEquals
import org.junit.Test

class OwnNumberSettingsStateTest {
    private val route = SimRoute("android-subscription-7", 7, "Default SMS SIM")

    @Test
    fun loadingBeforeFirstDetection() {
        assertEquals(
            OwnNumberUiState.Loading,
            ownNumberUiState(true, route, null, null),
        )
    }

    @Test
    fun savedManualValueIsAuthoritativeWhenCarrierNumberIsDetected() {
        assertEquals(
            OwnNumberUiState.Detected("+15551234567", "+15557654321"),
            ownNumberUiState(true, route, OwnNumberDetection.Detected("+15551234567"), "+15557654321"),
        )
    }

    @Test
    fun noAccessRetainsSavedValueWhenPresent() {
        assertEquals(
            OwnNumberUiState.NoAccess("+15551234567"),
            ownNumberUiState(true, route, OwnNumberDetection.NoAccess, "+15551234567"),
        )
    }

    @Test
    fun noAccessWithoutSavedValueIsStillNoAccess() {
        assertEquals(OwnNumberUiState.NoAccess(null), ownNumberUiState(true, route, OwnNumberDetection.NoAccess, null))
    }

    @Test
    fun unavailableDetectionUsesManualState() {
        assertEquals(OwnNumberUiState.Manual(null), ownNumberUiState(true, route, OwnNumberDetection.Unavailable, null))
        assertEquals(OwnNumberUiState.Manual("+15551234567"), ownNumberUiState(true, route, OwnNumberDetection.Unavailable, "+15551234567"))
    }

    @Test
    fun unavailableWhenDatabaseClosedOrNoRoute() {
        assertEquals(OwnNumberUiState.Unavailable, ownNumberUiState(false, route, OwnNumberDetection.Unavailable, null))
        assertEquals(OwnNumberUiState.Unavailable, ownNumberUiState(true, null, OwnNumberDetection.Unavailable, null))
    }
}
