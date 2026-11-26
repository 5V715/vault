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
import kotlinx.coroutines.Dispatchers
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

/** The prompt can't be passed now: [locked] after too many attempts, else no screen lock. */
class PresenceUnavailableException(val locked: Boolean) :
    Exception(if (locked) "too many attempts" else "no screen lock")

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

    /**
     * A new vault whose recovery key is being shown and checked. It is not
     * complete (not listed, swept at the next start) until [confirmCreated].
     */
    class Created(val dir: VaultDir, val identity: VaultIdentity, val recoveryKey: String)

    /**
     * Creates a vault for [identity]. Order: K_bg (vault-ffi saves the first
     * high-water mark during create), the container, the manifest key sealed,
     * K_auth, the CRK wrapped after a BiometricPrompt. The identity file, which
     * marks the vault complete, is written by [confirmCreated] once the user
     * has confirmed the recovery key. Any failure or cancellation deletes
     * everything written so far.
     */
    suspend fun create(identity: VaultIdentity, presence: UserPresence): Created = withContext(Dispatchers.IO) {
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
            Created(dir, identity, created.recoveryKey)
        } catch (t: Throwable) {
            discard(dir)
            throw t
        }
    }

    /** The user confirmed the recovery key: the vault becomes complete and listed. */
    suspend fun confirmCreated(created: Created) = withContext(Dispatchers.IO) {
        if (storage.exists(created.identity)) throw DuplicateVaultException(created.identity)
        storage.writeIdentity(created.dir, created.identity)
    }

    /** The user gave up on a new vault before confirming its recovery key. */
    suspend fun discardCreated(created: Created) {
        discard(created.dir)
    }

    /** Deletes a partial vault; never replaces the error that led here. */
    private suspend fun discard(dir: VaultDir) = withContext(NonCancellable + Dispatchers.IO) {
        runCatching { storage.delete(dir) }
    }

    /**
     * Restores a vault from [source] with [recoveryKey]. Cancelling the
     * coroutine aborts vault-ffi's restore (it removes its staging directory)
     * and deletes the vault directory and keys. The container is complete
     * before K_auth is created; K_bg exists from the start because vault-ffi
     * saves the high-water mark during the restore.
     */
    suspend fun restore(source: ReplicaTarget, recoveryKey: String, presence: UserPresence): VaultIdentity = withContext(Dispatchers.IO) {
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
            identity
        } catch (t: Throwable) {
            discard(dir)
            throw t
        }
    }

    /** Whether [dir]'s K_auth is gone or invalidated: "Re-wrap with recovery key". */
    suspend fun needsRewrap(dir: VaultDir): Boolean = withContext(Dispatchers.IO) {
        // A re-wrap that never finished (process killed at the prompt) left a
        // new K_auth that does not open the old wrapped file.
        dir.rewrapPending.exists() || !keys.isAuthKeyUsable(dir.id)
    }

    /**
     * Re-wrap after K_auth was invalidated: the recovery key opens the local
     * container (against its high-water mark), then a new K_auth (replacing the
     * invalidated one) wraps the CRK. If the prompt is cancelled or anything
     * fails, the new K_auth is deleted again, so the vault still shows
     * "Re-wrap" (the wrapped file is replaced only on success).
     */
    suspend fun rewrap(dir: VaultDir, recoveryKey: String, presence: UserPresence) = withContext(Dispatchers.IO) {
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
        // Held outside withContext: if the caller is cancelled after the
        // decryption, withContext throws on the way back and its result
        // would be dropped without being wiped.
        var crk: ByteArray? = null
        try {
            withContext(Dispatchers.IO) {
                val wrapped = dir.wrappedCrk.readBytes()
                val cipher = presence.authenticate(UNLOCK_TITLE, keys.authCipherForUnwrap(dir.id, wrapped))
                    ?: throw PresenceCancelledException()
                crk = keys.unwrapCrk(dir.id, cipher, wrapped)
            }
            return checkNotNull(crk)
        } catch (t: Throwable) {
            crk?.fill(0)
            throw t
        }
    }

    /** Opens [dir] locked, for stats; the caller closes the handle. */
    suspend fun open(dir: VaultDir): VaultHandle = factory.open(dir.container.path, storage.highWaterStore(dir))

    suspend fun stats(dir: VaultDir): VaultStats = open(dir).use { it.stats() }

    /** Deletes a vault and its keys. The UI asks for auth and the typed app id first. */
    suspend fun delete(dir: VaultDir) = withContext(Dispatchers.IO) { storage.delete(dir) }

    private suspend fun wrapAndStoreCrk(dir: VaultDir, crk: ByteArray, presence: UserPresence) {
        // Marks "K_auth and crk.wrapped may not match" until the new file is in
        // place, even if the process dies in between (then needsRewrap is true).
        writeAtomically(dir.rewrapPending, ByteArray(0))
        keys.createAuthKey(dir.id)
        try {
            val cipher = presence.authenticate(PROTECT_TITLE, keys.authCipherForWrap(dir.id))
                ?: throw PresenceCancelledException()
            writeAtomically(dir.wrappedCrk, keys.wrapCrk(dir.id, cipher, crk))
            if (!dir.rewrapPending.delete()) throw java.io.IOException("cannot clear the re-wrap marker")
        } catch (t: Throwable) {
            // The wrapped file (if any) is under the old key: K_auth must not
            // look usable, or the vault would hide its re-wrap entry.
            withContext(NonCancellable) { runCatching { keys.deleteAuthKey(dir.id) } }
            throw t
        }
    }

    override fun close() = factory.close()

    companion object {
        const val PROTECT_TITLE = "Protect the vault with your screen lock"
        const val UNLOCK_TITLE = "Unlock the vault"
    }
}
