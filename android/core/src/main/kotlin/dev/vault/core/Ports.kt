// SPDX-License-Identifier: MIT
package dev.vault.core

/**
 * The vaults on this phone, one per [VaultIdentity] (the app keeps each under
 * `noBackupFilesDir/vaults/<random id>/` with its identity sealed; tests fake it).
 */
interface VaultRepository {
    fun list(): List<VaultIdentity>

    fun exists(identity: VaultIdentity): Boolean
}

/**
 * The per-vault Keystore keys, apart from K_auth's authenticated use (that runs
 * through a BiometricPrompt `CryptoObject` in the app, so it cannot be a plain
 * call). The app implements it on AndroidKeyStore; tests fake it.
 *
 * Callers wipe every plaintext `ByteArray` they pass in or get back
 * (`fill(0)`) as soon as they are done with it.
 */
interface VaultKeyStore {
    /** Creates K_auth and K_bg for [vaultId]; replaces neither if they exist. */
    fun createKeys(vaultId: String)

    /** Deletes both keys (vault deletion, or before a re-wrap creates new ones). */
    fun deleteKeys(vaultId: String)

    /** False when K_auth is missing or permanently invalidated (new biometric). */
    fun isAuthKeyUsable(vaultId: String): Boolean

    /** Seals [plaintext] with K_bg (AES-256-GCM, fresh nonce). */
    fun sealBackground(vaultId: String, plaintext: ByteArray): ByteArray

    /** Opens what [sealBackground] sealed; throws when the key is gone or the data was altered. */
    fun openBackground(vaultId: String, sealed: ByteArray): ByteArray
}

/** View-model logic over [VaultRepository]: the list, offer lookup, create checks. */
class VaultCatalog(private val repository: VaultRepository) {
    /** The vaults, sorted by app id, then origin. */
    fun list(): List<VaultIdentity> = repository.list().sortedWith(compareBy({ it.appId }, { it.origin }))

    /**
     * The vault an offer belongs to: exactly its `(appId, origin)`. Offers carry
     * the canonical form (vault-ffi validated it), so a non-canonical one matches
     * nothing rather than being normalized into a match.
     */
    fun vaultFor(offer: Offer): VaultIdentity? {
        val id = (VaultIdentity.of(offer.appId, offer.origin) as? Checked.Valid)?.value ?: return null
        if (id.origin != offer.origin) return null
        return id.takeIf { repository.exists(it) }
    }

    /** Validates a new vault's identity from the create form. */
    fun checkNew(appId: String, origin: String): Checked<VaultIdentity> =
        when (val id = VaultIdentity.of(appId.trim(), origin.trim())) {
            is Checked.Invalid -> id
            is Checked.Valid ->
                if (repository.exists(id.value)) {
                    Checked.Invalid("A vault for ${id.value.appId} at ${id.value.origin} already exists")
                } else {
                    id
                }
        }
}
