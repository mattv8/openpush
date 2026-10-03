package dev.peppy.mobile

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class OwnNumberDetectorTest {
    @Test
    fun normalizeFormatsUsAndKeepsValidE164() {
        assertEquals(
            "+15555550100",
            OwnNumberDetector.normalizeOwnNumber("(555) 555-0100", "us") { raw, country ->
                if (raw == "(555) 555-0100" && country == "US") "+15555550100" else null
            },
        )
        assertEquals("+15555550100", OwnNumberDetector.normalizeOwnNumber("+15555550100", "us"))
    }

    @Test
    fun normalizeRejectsInvalidValuesAndFallsBackToStrippingPresentationCharacters() {
        assertEquals("+15555550100", OwnNumberDetector.normalizeOwnNumber("+1 (555) 555-0100", "us") { _, _ -> null })
        listOf<String?>(null, "", "hello", "123456", "+1234567890123456").forEach { raw ->
            assertNull(OwnNumberDetector.normalizeOwnNumber(raw, "us"))
        }
    }

    @Test
    fun detectRequiresAccessAndClassifiesUnavailableSources() {
        val route = SimRoute("android-subscription-7", 7, "Default SMS SIM")
        assertEquals(
            OwnNumberDetection.NoAccess,
            OwnNumberDetector.detect(null, route, OwnNumberSource { "+15555550100" }, access = { false }),
        )
        assertEquals(
            OwnNumberDetection.Unavailable,
            OwnNumberDetector.detect(null, route, OwnNumberSource { "" }, access = { true }),
        )
        listOf<RuntimeException>(SecurityException(), IllegalStateException(), UnsupportedOperationException()).forEach { error ->
            assertEquals(
                OwnNumberDetection.Unavailable,
                OwnNumberDetector.detect(null, route, OwnNumberSource { throw error }, access = { true }),
            )
        }
        assertEquals(
            OwnNumberDetection.Detected("+15555550100"),
            OwnNumberDetector.detect(
                null,
                route,
                OwnNumberSource { "(555) 555-0100" },
                access = { true },
                countryIso = { "us" },
                toE164 = { _, _ -> "+15555550100" },
            ),
        )
    }

    @Test
    fun syncWritesOnlyDetectedNumbers() {
        val route = SimRoute("android-subscription-7", 7, "Default SMS SIM")
        val writes = mutableListOf<Pair<String, String>>()
        assertEquals(
            OwnNumberDetection.Detected("+15555550100"),
            OwnNumberDetector.sync(route, { OwnNumberDetection.Detected("+15555550100") }, saved = { null }) { routeId, number ->
                writes += routeId to number
            },
        )
        assertEquals(listOf("android-subscription-7" to "+15555550100"), writes)
        assertEquals(OwnNumberDetection.Unavailable, OwnNumberDetector.sync(route, { OwnNumberDetection.Unavailable }, saved = { null }) { _, _ -> error("must not write") })
        assertNull(OwnNumberDetector.sync(null, { OwnNumberDetection.Detected("+15555550100") }, saved = { null }) { _, _ -> error("must not write") })
    }

    @Test
    fun existingManualNumberSurvivesUiReloadAndBackgroundDetection() {
        val route = SimRoute("android-subscription-7", 7, "Default SMS SIM")
        val saved = "+15557654321"
        var writes = 0

        assertEquals(
            saved,
            OwnNumberDetector.seedDetectedNumber(saved, OwnNumberDetection.Detected("+15555550100")) { writes++ },
        )
        assertEquals(
            OwnNumberDetection.Detected("+15555550100"),
            OwnNumberDetector.sync(
                route,
                { OwnNumberDetection.Detected("+15555550100") },
                saved = { saved },
            ) { _, _ -> writes++ },
        )
        assertEquals(0, writes)
        assertEquals(
            OwnNumberUiState.Detected("+15555550100", saved),
            ownNumberUiState(true, route, OwnNumberDetection.Detected("+15555550100"), saved),
        )
    }

    @Test
    fun mmsCaptureOptOutOrDeniedPermissionDoesNotRunDetection() {
        var detectorCalls = 0
        var writerCalls = 0
        val detectorAndWriter: () -> Unit = { detectorCalls++; writerCalls++; Unit }

        assertFalse(MmsCapture.syncOwnNumberForMmsCapture(false, { error("permission must not be read") }, { error("policy must not be read") }, detectorAndWriter))
        assertFalse(MmsCapture.syncOwnNumberForMmsCapture(true, { false }, { error("policy must not be read") }, detectorAndWriter))
        assertFalse(MmsCapture.syncOwnNumberForMmsCapture(true, { true }, { false }, detectorAndWriter))
        assertEquals(0, detectorCalls)
        assertEquals(0, writerCalls)
        assertTrue(MmsCapture.syncOwnNumberForMmsCapture(true, { true }, { true }, detectorAndWriter))
        assertEquals(1, detectorCalls)
        assertEquals(1, writerCalls)
    }
}
