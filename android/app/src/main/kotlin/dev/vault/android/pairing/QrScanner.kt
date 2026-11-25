// SPDX-License-Identifier: MIT
package dev.vault.android.pairing

import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import com.google.zxing.BarcodeFormat
import com.google.zxing.BinaryBitmap
import com.google.zxing.DecodeHintType
import com.google.zxing.PlanarYUVLuminanceSource
import com.google.zxing.ReaderException
import com.google.zxing.common.HybridBinarizer
import com.google.zxing.qrcode.QRCodeReader

/** QR codes from 8-bit luminance images (ZXing core; no Play Services, no ML Kit). */
object QrDecoder {
    private val hints: Map<DecodeHintType, Any> = mapOf(
        DecodeHintType.POSSIBLE_FORMATS to listOf(BarcodeFormat.QR_CODE),
        DecodeHintType.TRY_HARDER to true,
    )

    /**
     * The text of the QR code in [luma] ([width] x [height] pixels, rows
     * [rowStride] bytes apart), or null if there is none. What the text means
     * is decided later (`PairingCode.classify`).
     */
    fun decode(luma: ByteArray, rowStride: Int, width: Int, height: Int): String? {
        if (width <= 0 || height <= 0 || rowStride < width || luma.size < rowStride * (height - 1) + width) return null
        return try {
            val source = PlanarYUVLuminanceSource(luma, rowStride, height, 0, 0, width, height, false)
            QRCodeReader().decode(BinaryBitmap(HybridBinarizer(source)), hints).text
        } catch (e: ReaderException) {
            null
        } catch (e: IllegalArgumentException) {
            null
        }
    }
}

/**
 * CameraX frame analyzer: decodes the Y plane of each frame (YUV_420_888)
 * and reports any QR text to [onText] (on the analysis thread).
 */
class QrAnalyzer(private val onText: (String) -> Unit) : ImageAnalysis.Analyzer {
    private var luma = ByteArray(0)

    override fun analyze(image: ImageProxy) {
        try {
            val plane = image.planes[0]
            if (plane.pixelStride != 1) return
            val data = plane.buffer
            data.rewind()
            val n = data.remaining()
            if (luma.size < n) luma = ByteArray(n)
            data.get(luma, 0, n)
            QrDecoder.decode(luma, plane.rowStride, image.width, image.height)?.let(onText)
        } finally {
            image.close()
        }
    }
}
