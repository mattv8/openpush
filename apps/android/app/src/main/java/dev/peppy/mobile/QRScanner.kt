package dev.peppy.mobile

import android.annotation.SuppressLint
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.google.mlkit.vision.barcode.BarcodeScanning
import com.google.mlkit.vision.barcode.common.Barcode

/** CameraX viewfinder that owns and closes its analyzer when it leaves composition. */
@SuppressLint("UnsafeOptInUsageError")
@Composable
internal fun QRScanner(modifier: Modifier = Modifier, onCode: (String) -> Unit) {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val executor = remember(context) { ContextCompat.getMainExecutor(context) }
    val scanner = remember { BarcodeScanning.getClient() }
    val cameraProvider = remember { ProcessCameraProvider.getInstance(context) }
    val previewView = remember(context) { PreviewView(context) }

    DisposableEffect(cameraProvider, lifecycleOwner, previewView) {
        var disposed = false
        val listener = Runnable {
            if (disposed) return@Runnable
            val provider = cameraProvider.get()
            val preview = Preview.Builder().build().also { it.setSurfaceProvider(previewView.surfaceProvider) }
            val analysis = ImageAnalysis.Builder()
                .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                .build()
            analysis.setAnalyzer(executor) { image ->
                val mediaImage = image.image
                if (mediaImage == null) {
                    image.close()
                    return@setAnalyzer
                }
                val input = com.google.mlkit.vision.common.InputImage.fromMediaImage(mediaImage, image.imageInfo.rotationDegrees)
                scanner.process(input)
                    .addOnSuccessListener { codes ->
                        codes.firstOrNull { it.format == Barcode.FORMAT_QR_CODE && !it.rawValue.isNullOrBlank() }
                            ?.rawValue?.let(onCode)
                    }
                    .addOnCompleteListener { image.close() }
            }
            provider.unbindAll()
            provider.bindToLifecycle(lifecycleOwner, CameraSelector.DEFAULT_BACK_CAMERA, preview, analysis)
        }
        cameraProvider.addListener(listener, executor)
        onDispose {
            disposed = true
            if (cameraProvider.isDone) cameraProvider.get().unbindAll()
            scanner.close()
        }
    }

    AndroidView(
        factory = { previewView },
        modifier = modifier.testTag("qr-viewfinder"),
    )
}
