// SPDX-License-Identifier: MIT
package dev.vault.android.keys

import android.content.Context
import android.content.pm.PackageManager
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import android.security.keystore.StrongBoxUnavailableException
import dev.vault.core.VaultKeyStore
import java.security.GeneralSecurityException
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** K_auth needs re-wrapping: the screen lock was removed (or the key is gone). */
class AuthKeyInvalidatedException(cause: Throwable? = null) :
    GeneralSecurityException("the vault's unlock key was invalidated; re-wrap with the recovery key", cause)

/**
 * The two per-vault AndroidKeyStore keys (spec "Keystore"):
 *
 * - **K_auth** (`vault.<id>.auth`) wraps the CRK. AES-256-GCM, every use needs a
 *   BiometricPrompt (BIOMETRIC_STRONG or the device credential, timeout 0), the
 *   device unlocked; invalidated when the screen lock is removed (not by a
 *   new biometric: a key the PIN also unlocks stays valid, and enrolling a
 *   biometric needs the PIN anyway; owner decision 2026-11-21);
 *   StrongBox when available.
 * - **K_bg** (`vault.<id>.bg`) seals the high-water mark, the manifest key, the
 *   replica credentials and the vault identity. AES-256-GCM, no user
 *   authentication, but only while the device is unlocked.
 *
 * Sealed blobs: `0x01 || 12-byte IV || ciphertext+tag`, with the AAD
 * `vault/android/v1/<kind>/<vault id>`, so a blob can't be moved to another
 * vault or used as another kind of file.
 *
 * [requireAuth] = false exists only for instrumented tests, which cannot pass a
 * BiometricPrompt: K_auth is then created without user authentication. That gap
 * is covered by the owner's manual checklist on a real phone.
 */
class VaultKeys(
    context: Context,
    private val requireAuth: Boolean = true,
) : VaultKeyStore {
    private val strongBox = context.packageManager.hasSystemFeature(PackageManager.FEATURE_STRONGBOX_KEYSTORE)
    private val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }

    override fun createKeys(vaultId: String) {
        createBackgroundKey(vaultId)
        createAuthKey(vaultId)
    }

    /** Creates K_bg (needed before vault-ffi saves the first high-water mark). */
    fun createBackgroundKey(vaultId: String) {
        if (keyStore.containsAlias(bgAlias(vaultId))) return
        generate(bgAlias(vaultId)) {
            setUnlockedDeviceRequired(true)
        }
    }

    /** Creates K_auth, replacing an existing (e.g. invalidated) one. */
    fun createAuthKey(vaultId: String) {
        keyStore.deleteEntry(authAlias(vaultId))
        generate(authAlias(vaultId)) {
            setUnlockedDeviceRequired(true)
            if (requireAuth) {
                setUserAuthenticationRequired(true)
                setUserAuthenticationParameters(
                    0,
                    KeyProperties.AUTH_BIOMETRIC_STRONG or KeyProperties.AUTH_DEVICE_CREDENTIAL,
                )
            }
        }
    }

    override fun deleteKeys(vaultId: String) {
        keyStore.deleteEntry(authAlias(vaultId))
        keyStore.deleteEntry(bgAlias(vaultId))
    }

    override fun isAuthKeyUsable(vaultId: String): Boolean = try {
        Cipher.getInstance(TRANSFORMATION).init(Cipher.ENCRYPT_MODE, key(authAlias(vaultId)))
        true
    } catch (e: GeneralSecurityException) {
        // Missing, or KeyPermanentlyInvalidatedException. A key that merely
        // needs authentication still initializes.
        false
    }

    /**
     * A cipher to wrap the CRK with K_auth. With [requireAuth] it must pass a
     * BiometricPrompt `CryptoObject` before [wrapCrk].
     */
    fun authCipherForWrap(vaultId: String): Cipher = authCipher {
        it.init(Cipher.ENCRYPT_MODE, key(authAlias(vaultId)))
    }

    /** A cipher to unwrap [wrapped]; must pass a BiometricPrompt before [unwrapCrk]. */
    fun authCipherForUnwrap(vaultId: String, wrapped: ByteArray): Cipher = authCipher {
        it.init(Cipher.DECRYPT_MODE, key(authAlias(vaultId)), GCMParameterSpec(TAG_BITS, iv(wrapped)))
    }

    /** Wraps [crk] with an authenticated cipher from [authCipherForWrap]. */
    fun wrapCrk(vaultId: String, cipher: Cipher, crk: ByteArray): ByteArray =
        finishSeal(cipher, aad(KIND_CRK, vaultId), crk)

    /** Unwraps with an authenticated cipher from [authCipherForUnwrap]. Wipe the result. */
    fun unwrapCrk(vaultId: String, cipher: Cipher, wrapped: ByteArray): ByteArray =
        finishOpen(cipher, aad(KIND_CRK, vaultId), wrapped)

    override fun sealBackground(vaultId: String, plaintext: ByteArray): ByteArray =
        sealBackground(vaultId, KIND_BG, plaintext)

    override fun openBackground(vaultId: String, sealed: ByteArray): ByteArray =
        openBackground(vaultId, KIND_BG, sealed)

    /** Seals a K_bg file of [kind] (its name), bound to the kind and the vault. */
    fun sealBackground(vaultId: String, kind: String, plaintext: ByteArray): ByteArray {
        val c = Cipher.getInstance(TRANSFORMATION)
        c.init(Cipher.ENCRYPT_MODE, key(bgAlias(vaultId)))
        return finishSeal(c, aad(kind, vaultId), plaintext)
    }

    fun openBackground(vaultId: String, kind: String, sealed: ByteArray): ByteArray {
        val c = Cipher.getInstance(TRANSFORMATION)
        c.init(Cipher.DECRYPT_MODE, key(bgAlias(vaultId)), GCMParameterSpec(TAG_BITS, iv(sealed)))
        return finishOpen(c, aad(kind, vaultId), sealed)
    }

    /** The key's alias names, for tests (KeyInfo checks). */
    fun authAlias(vaultId: String) = "vault.$vaultId.auth"

    fun bgAlias(vaultId: String) = "vault.$vaultId.bg"

    private inline fun authCipher(init: (Cipher) -> Unit): Cipher {
        val c = Cipher.getInstance(TRANSFORMATION)
        try {
            init(c)
        } catch (e: KeyPermanentlyInvalidatedException) {
            throw AuthKeyInvalidatedException(e)
        }
        return c
    }

    private fun key(alias: String): SecretKey =
        (keyStore.getKey(alias, null) as? SecretKey) ?: throw AuthKeyInvalidatedException()

    private fun generate(alias: String, configure: KeyGenParameterSpec.Builder.() -> Unit) {
        fun spec(strong: Boolean) = KeyGenParameterSpec.Builder(
            alias,
            KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
            .setKeySize(256)
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setRandomizedEncryptionRequired(true)
            .setIsStrongBoxBacked(strong)
            .apply(configure)
            .build()
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE)
        try {
            gen.init(spec(strongBox))
            gen.generateKey()
        } catch (e: StrongBoxUnavailableException) {
            gen.init(spec(false))
            gen.generateKey()
        }
    }

    private fun finishSeal(cipher: Cipher, aad: ByteArray, plaintext: ByteArray): ByteArray {
        cipher.updateAAD(aad)
        val ct = cipher.doFinal(plaintext)
        val iv = cipher.iv
        check(iv.size == IV_BYTES) { "unexpected GCM IV length" }
        return byteArrayOf(FORMAT) + iv + ct
    }

    private fun finishOpen(cipher: Cipher, aad: ByteArray, sealed: ByteArray): ByteArray {
        cipher.updateAAD(aad)
        return cipher.doFinal(sealed, 1 + IV_BYTES, sealed.size - 1 - IV_BYTES)
    }

    private fun iv(sealed: ByteArray): ByteArray {
        if (sealed.size < 1 + IV_BYTES + TAG_BITS / 8 || sealed[0] != FORMAT) {
            throw GeneralSecurityException("malformed sealed blob")
        }
        return sealed.copyOfRange(1, 1 + IV_BYTES)
    }

    private fun aad(kind: String, vaultId: String) =
        "vault/android/v1/$kind/$vaultId".toByteArray(Charsets.US_ASCII)

    companion object {
        const val ANDROID_KEYSTORE = "AndroidKeyStore"
        private const val TRANSFORMATION = "AES/GCM/NoPadding"
        private const val TAG_BITS = 128
        private const val IV_BYTES = 12
        private const val FORMAT: Byte = 1
        private const val KIND_CRK = "crk"
        private const val KIND_BG = "bg"
    }
}
