package dev.peppy.mobile

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.peppy_mobile_bindings.MobileBindingsException

class MmsSettingsStateTest {
    @Test
    fun ownNumberErrorsUseSafeActionableCodesRatherThanExceptionText() {
        assertEquals(
            "This number could not be saved for the selected SIM.",
            ownAddressError(MobileBindingsException.InvalidRequest()),
        )
        val generic = ownAddressError(IllegalStateException("/private/secret database path"))
        assertEquals("Could not save the number securely. Try again.", generic)
        assertFalse(generic.contains("secret"))
    }

    @Test
    fun acquisitionHealthSeparatesDatabaseUnavailableFromNoPendingRows() {
        assertTrue(MmsHealth(databaseUnavailable = true).databaseUnavailable)
        assertFalse(MmsHealth().databaseUnavailable)
    }
}
