// SPDX-License-Identifier: MIT
package dev.vault.android.vault

import dev.vault.android.keys.AuthKeyInvalidatedException
import dev.vault.android.keys.VaultKeys
import dev.vault.android.storage.ReplicaConfig
import dev.vault.android.storage.VaultDir
import dev.vault.android.storage.VaultStorage
import dev.vault.android.storage.writeAtomically
import dev.vault.core.Checked
import dev.vault.core.LockSettings
import dev.vault.core.VaultIdentity
import javax.crypto.Cipher
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.withContext
import uniffi.vault_ffi.FactoryConfig
import uniffi.vault_ffi.NamedReplica
import uniffi.vault_ffi.ReplicaTarget
import uniffi.vault_ffi.VaultFactory
import uniffi.vault_ffi.VaultHandle
import uniffi.vault_ffi.VaultStats

/**
 * Shows the BiometricPrompt with [cipher] as its CryptoObject and returns the
 * authenticated cipher, or null if the user cancelled or authentication failed.
 */
interface UserPresence {
    suspend fun authenticate(title: String, cipher: Cipher): Cipher?

    /** The same prompt without a key, for confirmations (delete); true if passed. */
    suspend fun confirm(title: String): Boolean
}

/** The user cancelled the BiometricPrompt: nothing was changed or kept. */
class PresenceCancelledException : Exception("cancelled at the unlock prompt")

/** A vault for this app and origin already exists on the phone. */
class DuplicateVaultException(val identity: VaultIdentity) :
    IllegalStateException("a vault for ${identity.appId} at ${identity.origin} already exists")

/**
 * Creates, restores, re-wraps and unlocks vaults: vault-ffi plus the Keystore
 * keys and sealed files. Every plaintext secret (CRK, manifest key) is wiped as
 * soon as it has been wrapped or handed over.
 */
class VaultManager(
    private val storage: VaultStorage,
    private val keys: VaultKeys,
    lockSettings: LockSettings = LockSettings(),
    extraCaPem: String? = null,
) : AutoCloseable {
    val factory = VaultFactory(
        FactoryConfig(
            extraCaPem = extraCaPem,
            approvalTimeoutSecs = 60uL,
            idleTimeoutSecs = lockSettings.idleTimeoutSecs.toULong(),
        ),
    )

    /** A new vault: what the create screen shows next. Drop it after the check. */
    class Created(val dir: VaultDir, val identity: VaultIdentity, val recoveryKey: String)

    /**
     * Creates a vault for [identity]. Order: K_bg (vault-ffi saves the first
     * high-water mark during create), the container, the manifest key sealed,
     * K_auth, the CRK wrapped after a BiometricPrompt, the identity file last
     * (it marks the vault complete). Any failure or cancellation deletes
     * everything written so far.
     */
    suspend fun create(identity: VaultIdentity, presence: UserPresence): Created {
        if (storage.exists(identity)) throw DuplicateVaultException(identity)
        val dir = storage.newVaultDir()
        try {
            keys.createBackgroundKey(dir.id)
            val created = factory.create(dir.container.path, identity.appId, identity.origin, storage.highWaterStore(dir))
            try {
                created.handle.lock()
                storage.writeSealed(dir, VaultDir.KIND_MANIFEST_KEY, created.manifestKey)
                wrapAndStoreCrk(dir, created.crk, presence)
            } finally {
                created.crk.fill(0)
                created.manifestKey.fill(0)
                created.handle.close()
            }
            storage.writeIdentity(dir, identity)
            return Created(dir, identity, created.recoveryKey)
        } catch (t: Throwable) {
            withContext(NonCancellable) { storage.delete(dir) }
            throw t
        }
    }

    /**
     * Restores a vault from [source] with [recoveryKey]. Cancelling the
     * coroutine aborts vault-ffi's restore (it removes its staging directory)
     * and deletes the vault directory and keys. The container is complete
     * before K_auth is created; K_bg exists from the start because vault-ffi
     * saves the high-water mark during the restore.
     */
    suspend fun restore(source: ReplicaTarget, recoveryKey: String, presence: UserPresence): VaultIdentity {
        val dir = storage.newVaultDir()
        // The replica's stable id: its anti-rollback mark and config entry use it.
        val sourceId = storage.newReplicaId()
        try {
            keys.createBackgroundKey(dir.id)
            val restored = factory.restore(dir.container.path, source, sourceId, recoveryKey, storage.highWaterStore(dir))
            val identity = restored.handle.use { handle ->
                val b = handle.binding()
                val id = (VaultIdentity.of(b.appId, b.origin) as? Checked.Valid)?.value
                    ?: throw IllegalStateException("the restored vault has an invalid binding")
                if (storage.exists(id)) throw DuplicateVaultException(id)
                storage.writeSealed(dir, VaultDir.KIND_MANIFEST_KEY, handle.exportManifestKey())
                // The credentials typed for this restore, kept for background replication.
                storage.writeSealed(dir, VaultDir.KIND_REPLICAS, ReplicaConfig.encode(listOf(NamedReplica(sourceId, source))))
                handle.lock()
                id
            }
            // The CRK from the recovery key, wrapped under a new K_auth.
            val crk = factory.rewrapWithRecoveryKey(dir.container.path, recoveryKey, storage.highWaterStore(dir))
            try {
                wrapAndStoreCrk(dir, crk, presence)
            } finally {
                crk.fill(0)
            }
            storage.writeIdentity(dir, identity)
            return identity
        } catch (t: Throwable) {
            withContext(NonCancellable) { storage.delete(dir) }
            throw t
        }
    }

    /** Whether [dir]'s K_auth still works; false means "Re-wrap with recovery key". */
    fun needsRewrap(dir: VaultDir): Boolean = !keys.isAuthKeyUsable(dir.id)

    /**
     * Re-wrap after K_auth was invalidated: the recovery key opens the local
     * container (against its high-water mark), then a new K_auth (replacing the
     * invalidated one) wraps the CRK. Cancelling at the prompt leaves the vault
     * needing a re-wrap, as before.
     */
    suspend fun rewrap(dir: VaultDir, recoveryKey: String, presence: UserPresence) {
        val crk = factory.rewrapWithRecoveryKey(dir.container.path, recoveryKey, storage.highWaterStore(dir))
        try {
            wrapAndStoreCrk(dir, crk, presence)
        } finally {
            crk.fill(0)
        }
    }

    /**
     * The CRK, after a BiometricPrompt on K_auth. The caller hands it to
     * vault-ffi and wipes it at once. Throws [AuthKeyInvalidatedException] when
     * a re-wrap is needed, [PresenceCancelledException] on cancel.
     */
    suspend fun unwrapCrk(dir: VaultDir, presence: UserPresence): ByteArray {
        val wrapped = dir.wrappedCrk.readBytes()
        val cipher = presence.authenticate(UNLOCK_TITLE, keys.authCipherForUnwrap(dir.id, wrapped))
            ?: throw PresenceCancelledException()
        return keys.unwrapCrk(dir.id, cipher, wrapped)
    }

    /** Opens [dir] locked, for stats; the caller closes the handle. */
    suspend fun open(dir: VaultDir): VaultHandle = factory.open(dir.container.path, storage.highWaterStore(dir))

    suspend fun stats(dir: VaultDir): VaultStats = open(dir).use { it.stats() }

    /** Deletes a vault and its keys. The UI asks for auth and the typed app id first. */
    fun delete(dir: VaultDir) = storage.delete(dir)

    private suspend fun wrapAndStoreCrk(dir: VaultDir, crk: ByteArray, presence: UserPresence) {
        keys.createAuthKey(dir.id)
        val cipher = presence.authenticate(PROTECT_TITLE, keys.authCipherForWrap(dir.id))
            ?: throw PresenceCancelledException()
        writeAtomically(dir.wrappedCrk, keys.wrapCrk(dir.id, cipher, crk))
    }

    override fun close() = factory.close()

    companion object {
        const val PROTECT_TITLE = "Protect the vault with your screen lock"
        const val UNLOCK_TITLE = "Unlock the vault"
    }
}
