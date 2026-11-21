// SPDX-License-Identifier: MIT
package dev.vault.android.vault

import android.content.Context
import android.hardware.biometrics.BiometricManager.Authenticators
import android.hardware.biometrics.BiometricPrompt
import android.os.CancellationSignal
import javax.crypto.Cipher
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
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

    /**
     * Shows the prompt (on the main thread). Returns the result, null if the
     * user cancelled; throws [PresenceUnavailableException] for a lockout or a
     * missing screen lock, so those aren't reported as "cancelled".
     */
    private suspend fun prompt(title: String, crypto: BiometricPrompt.CryptoObject?): BiometricPrompt.AuthenticationResult? =
        withContext(Dispatchers.Main) { show(title, crypto) }

    private suspend fun show(title: String, crypto: BiometricPrompt.CryptoObject?): BiometricPrompt.AuthenticationResult? =
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
                    if (!cont.isActive) return
                    when (errorCode) {
                        BiometricPrompt.BIOMETRIC_ERROR_LOCKOUT,
                        BiometricPrompt.BIOMETRIC_ERROR_LOCKOUT_PERMANENT,
                        -> cont.resumeWithException(PresenceUnavailableException(locked = true))
                        BiometricPrompt.BIOMETRIC_ERROR_NO_DEVICE_CREDENTIAL,
                        BiometricPrompt.BIOMETRIC_ERROR_HW_UNAVAILABLE,
                        BiometricPrompt.BIOMETRIC_ERROR_HW_NOT_PRESENT,
                        -> cont.resumeWithException(PresenceUnavailableException(locked = false))
                        // User or system cancel, timeout: treated as "cancelled".
                        else -> cont.resume(null)
                    }
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
