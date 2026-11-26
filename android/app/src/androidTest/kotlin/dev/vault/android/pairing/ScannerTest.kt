// SPDX-License-Identifier: MIT
package dev.vault.android.pairing

import android.content.Context
import android.graphics.BitmapFactory
import android.graphics.ImageFormat
import androidx.camera.core.ImageProxy
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import dev.vault.android.VaultApp
import dev.vault.android.keys.VaultKeys
import dev.vault.android.storage.VaultStorage
import dev.vault.android.vault.VaultManager
import dev.vault.core.LockSettings
import dev.vault.core.Offer
import dev.vault.core.PairingCode
import dev.vault.core.PairingMachine.State
import dev.vault.core.PairingOutcome
import java.io.File
import java.lang.reflect.Proxy
import java.nio.ByteBuffer
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

/** An interface implemented by [answer] (method name to result), for CameraX's frame types. */
private inline fun <reified T> fake(crossinline answer: (String) -> Any?): T =
    Proxy.newProxyInstance(T::class.java.classLoader, arrayOf(T::class.java)) { _, method, _ ->
        when (method.name) {
            "hashCode" -> 0
            "equals" -> false
            "toString" -> "fake ${T::class.java.simpleName}"
            else -> answer(method.name)
        }
    } as T

/** A YUV_420_888 camera frame whose Y plane is [luma], rows [rowStride] bytes apart. */
private fun frame(luma: ByteArray, width: Int, height: Int, rowStride: Int): ImageProxy {
    val plane = fake<ImageProxy.PlaneProxy> { name ->
        when (name) {
            "getBuffer" -> ByteBuffer.wrap(luma)
            "getRowStride" -> rowStride
            "getPixelStride" -> 1
            else -> null
        }
    }
    return fake<ImageProxy> { name ->
        when (name) {
            "getPlanes" -> arrayOf(plane)
            "getWidth" -> width
            "getHeight" -> height
            "getFormat" -> ImageFormat.YUV_420_888
            else -> null
        }
    }
}

/**
 * The scanner pipeline on fixture images (androidTest assets, made with a
 * standard QR encoder): CameraX analyzer → ZXing → pairing-code check → the
 * coordinator. The emulator has no camera, so the frames are built from the
 * fixtures; the CameraX preview itself is on the owner's manual checklist.
 */
@RunWith(AndroidJUnit4::class)
class ScannerTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context: Context = instrumentation.targetContext
    private lateinit var keys: VaultKeys
    private lateinit var manager: VaultManager
    private lateinit var coordinator: PairingCoordinator

    @Before
    fun setUp() {
        (context.applicationContext as VaultApp).sweepOnce()
        File(context.noBackupFilesDir, "vaults").deleteRecursively()
        keys = VaultKeys(context, requireAuth = false)
        val storage = VaultStorage(context, keys)
        manager = VaultManager(storage, keys)
        coordinator = PairingCoordinator(context, manager, storage, lockSettings = { LockSettings() })
    }

    @After
    fun tearDown() {
        manager.close()
    }

    /** The fixture's luminance, each row padded by [padding] bytes like a camera buffer. */
    private fun scanFixture(name: String, padding: Int = 0): String? {
        val bmp = checkNotNull(instrumentation.context.assets.open(name).use { BitmapFactory.decodeStream(it) }) { name }
        val w = bmp.width
        val h = bmp.height
        val stride = w + padding
        val pixels = IntArray(w * h)
        bmp.getPixels(pixels, 0, w, 0, 0, w, h)
        val luma = ByteArray(stride * h)
        for (y in 0 until h) {
            for (x in 0 until w) {
                val c = pixels[y * w + x]
                val r = (c shr 16) and 0xff
                val g = (c shr 8) and 0xff
                val b = c and 0xff
                luma[y * stride + x] = ((299 * r + 587 * g + 114 * b) / 1000).toByte()
            }
        }
        return analyze(frame(luma, w, h, stride))
    }

    private fun analyze(image: ImageProxy): String? {
        val texts = mutableListOf<String>()
        QrAnalyzer { texts += it }.analyze(image)
        assertTrue("one frame, at most one code: $texts", texts.size <= 1)
        return texts.singleOrNull()
    }

    @Test
    fun scanner_accepts_only_vault_pair() {
        // ZXing reads every fixture, with and without padded camera rows.
        val offer = scanFixture("qr/vault-pair.png")
        assertTrue("$offer", offer != null && offer.startsWith(PairingCode.PREFIX))
        assertEquals(offer, scanFixture("qr/vault-pair.png", padding = 64))
        val other = scanFixture("qr/not-a-vault.png")
        assertEquals("https://example.com/hello", other)
        val damaged = scanFixture("qr/vault-pair-damaged.png", padding = 16)
        assertEquals("vault-pair:v1:not*base64!", damaged)
        assertNull(analyze(frame(ByteArray(200 * 200) { -1 }, 200, 200, 200)))
        offer!!

        // Only the vault-pair:v1: code is accepted; the others keep the scanner going.
        assertEquals(PairingCode.Scan.Offer(offer), PairingCode.classify(offer))
        val info = manager.factory.parseOffer(offer)
        assertEquals("todo", info.appId)
        assertEquals("https://todo.example", info.origin)
        runBlocking {
            assertEquals(ScanResult.Rejected(PairingCode.Scan.NotAVaultCode.message), coordinator.scan(other!!))
            assertEquals(ScanResult.Rejected(PairingCode.Scan.Malformed.message), coordinator.scan(damaged!!))
            assertNull(coordinator.pairing.value)
            // A real offer, but this phone has no vault for it: "No vault for
            // this app", with the offer kept to pre-fill Create. Nothing dialed.
            assertEquals(ScanResult.Started, coordinator.scan(offer))
        }
        val view = coordinator.pairing.value
        val expected = State.Ended(null, PairingOutcome.NoVaultForApp(Offer(info.appId, info.origin, info.relayHost)))
        assertEquals(PairingView(null, expected), view)
        assertEquals("No vault for this app", expected.outcome.message)
        assertFalse("no service without a vault", SessionService.running.value)
    }
}
