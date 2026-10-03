package dev.peppy.mobile

import android.Manifest
import android.app.Application
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import java.io.File

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class MmsWiringTest {
    @Test
    fun mmsProviderPathIsConfinedToCarrierPduDirectory() {
        val xml = File("src/main/res/xml/mms_paths.xml").readText()
        assertTrue(xml.contains("<files-path name=\"mms\" path=\"mms-pdu/\""))
        assertFalse(xml.contains("root-path"))
        assertFalse(xml.contains("path=\".\""))
    }

    @Test
    fun mmsReceiveRequiresBothRuntimePermissions() {
        val context = ApplicationProvider.getApplicationContext<Application>()
        val app = shadowOf(context)
        app.denyPermissions(Manifest.permission.READ_SMS, Manifest.permission.RECEIVE_MMS)
        app.grantPermissions(Manifest.permission.SEND_SMS)
        assertFalse(mmsReceiveGranted(context))
        app.grantPermissions(Manifest.permission.READ_SMS)
        assertFalse(mmsReceiveGranted(context))
        app.grantPermissions(Manifest.permission.RECEIVE_MMS)
        assertTrue(mmsReceiveGranted(context))
        app.denyPermissions(Manifest.permission.READ_SMS)
        assertFalse(mmsReceiveGranted(context))
    }
}
