// SPDX-License-Identifier: MIT
package dev.vault.android.vault

import android.content.Context
import android.hardware.biometrics.BiometricManager.Authenticators
import android.hardware.biometrics.BiometricPrompt
import android.os.CancellationSignal
import javax.crypto.Cipher
import kotlin.coroutines.resume
import kotlinx.coroutines.suspendCancellableCoroutine

/**
 * [UserPresence] through the platform BiometricPrompt (API 30+ allows a
 * CryptoObject with BIOMETRIC_STRONG or the device credential, so no androidx
 * dependency is needed). Cancelling the coroutine cancels the prompt.
 */
class BiometricPresence(private val context: Context) : UserPresence {
    override suspend fun authenticate(title: String, cipher: Cipher): Cipher? =
        prompt(title, BiometricPrompt.CryptoObject(cipher))?.cryptoObject?.cipher

    override suspend fun confirm(title: String): Boolean = prompt(title, null) != null

    private suspend fun prompt(title: String, crypto: BiometricPrompt.CryptoObject?): BiometricPrompt.AuthenticationResult? =
        suspendCancellableCoroutine { cont ->
            val signal = CancellationSignal()
            cont.invokeOnCancellation { signal.cancel() }
            val builder = BiometricPrompt.Builder(context)
                .setTitle(title)
                .setAllowedAuthenticators(Authenticators.BIOMETRIC_STRONG or Authenticators.DEVICE_CREDENTIAL)
                .setConfirmationRequired(true)
            val callback = object : BiometricPrompt.AuthenticationCallback() {
                override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
                    if (cont.isActive) cont.resume(result)
                }

                override fun onAuthenticationError(errorCode: Int, errString: CharSequence) {
                    // Cancelled, lockout, no credential.
                    if (cont.isActive) cont.resume(null)
                }
                // onAuthenticationFailed: one bad attempt; the prompt stays open.
            }
            val prompt = builder.build()
            if (crypto != null) {
                prompt.authenticate(crypto, signal, context.mainExecutor, callback)
            } else {
                prompt.authenticate(signal, context.mainExecutor, callback)
            }
        }
}
