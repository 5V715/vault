// SPDX-License-Identifier: MIT
package dev.vault.android.ui

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.provider.Settings
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.viewinterop.AndroidView
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.LifecycleOwner
import dev.vault.android.pairing.PairingCoordinator
import dev.vault.android.pairing.QrAnalyzer
import dev.vault.android.pairing.ScanResult
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

private fun Context.granted(permission: String) =
    checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED

/**
 * The pairing-code scanner: camera permission (and, on Android 13+, the
 * notification permission for the session notification), a CameraX preview
 * and ZXing on every frame. Only `vault-pair:v1:` codes start a pairing;
 * anything else shows a message and scanning goes on.
 */
@Composable
fun ScanScreen(
    pairing: PairingCoordinator,
    lifecycleOwner: LifecycleOwner,
    onStarted: () -> Unit,
    onBack: () -> Unit,
) {
    val context = LocalContext.current
    var camera by remember { mutableStateOf(context.granted(Manifest.permission.CAMERA)) }
    var cameraDenied by remember { mutableStateOf(false) }
    var message by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    var cameraError by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val started by rememberUpdatedState(onStarted)

    val notifications = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {
        // Optional: without it the session notification is hidden, the service still runs.
    }
    fun askNotifications() {
        if (Build.VERSION.SDK_INT >= 33 && !context.granted(Manifest.permission.POST_NOTIFICATIONS)) {
            notifications.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
    }
    // Denied for good ("don't ask again"): the request returns at once, so offer the app's settings.
    var deniedForGood by remember { mutableStateOf(false) }
    val cameraPermission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { ok ->
        camera = ok
        cameraDenied = !ok
        deniedForGood = !ok && (context as? Activity)?.shouldShowRequestPermissionRationale(Manifest.permission.CAMERA) == false
        if (ok) askNotifications()
    }
    // Back from the settings: the permission may have been granted there.
    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME && !camera && context.granted(Manifest.permission.CAMERA)) {
                camera = true
                cameraDenied = false
                askNotifications()
            }
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose { lifecycleOwner.lifecycle.removeObserver(observer) }
    }
    LaunchedEffect(Unit) {
        if (camera) askNotifications() else cameraPermission.launch(Manifest.permission.CAMERA)
    }

    fun onText(text: String) {
        if (busy) return
        busy = true
        scope.launch {
            when (val r = pairing.scan(text)) {
                ScanResult.Started -> started()
                is ScanResult.Rejected -> {
                    message = r.message
                    delay(REJECT_PAUSE_MS)
                    busy = false
                }
            }
        }
    }

    VaultScaffold("Scan the pairing code", onBack = onBack) {
        Text(
            "Open the app in the browser and scan the code it shows.",
            style = MaterialTheme.typography.bodyLarge,
        )
        when {
            camera && !cameraError -> CameraPreview(lifecycleOwner, onText = ::onText, onError = { cameraError = true })
            cameraError -> ErrorText("No camera is available.")
            cameraDenied -> {
                ErrorText("The camera permission is needed to scan the pairing code.")
                if (deniedForGood) {
                    Button(onClick = {
                        context.startActivity(
                            Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.fromParts("package", context.packageName, null)),
                        )
                    }) { Text("Open the app's settings") }
                } else {
                    Button(onClick = { cameraPermission.launch(Manifest.permission.CAMERA) }) { Text("Allow the camera") }
                }
            }
        }
        message?.let { ErrorText(it) }
    }
}

@Composable
private fun CameraPreview(lifecycleOwner: LifecycleOwner, onText: (String) -> Unit, onError: () -> Unit) {
    val context = LocalContext.current
    val currentOnText by rememberUpdatedState(onText)
    val currentOnError by rememberUpdatedState(onError)
    val executor: ExecutorService = remember { Executors.newSingleThreadExecutor() }
    val main = remember { Handler(Looper.getMainLooper()) }
    val previewView = remember { PreviewView(context) }
    DisposableEffect(lifecycleOwner) {
        val future = ProcessCameraProvider.getInstance(context)
        var provider: ProcessCameraProvider? = null
        var disposed = false
        future.addListener(
            {
                if (disposed) return@addListener
                try {
                    val p = future.get()
                    provider = p
                    val preview = Preview.Builder().build()
                    preview.setSurfaceProvider(previewView.surfaceProvider)
                    val analysis = ImageAnalysis.Builder()
                        .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                        .build()
                    analysis.setAnalyzer(executor, QrAnalyzer { text -> main.post { currentOnText(text) } })
                    p.unbindAll()
                    p.bindToLifecycle(lifecycleOwner, CameraSelector.DEFAULT_BACK_CAMERA, preview, analysis)
                } catch (e: Exception) {
                    currentOnError()
                }
            },
            context.mainExecutor,
        )
        onDispose {
            disposed = true
            provider?.unbindAll()
            executor.shutdown()
        }
    }
    AndroidView(factory = { previewView }, modifier = Modifier.fillMaxWidth().aspectRatio(3f / 4f))
}

private const val REJECT_PAUSE_MS = 1500L
