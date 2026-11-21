// SPDX-License-Identifier: MIT
package dev.vault.android.storage

import android.content.Context
import dev.vault.android.keys.VaultKeys
import dev.vault.core.Checked
import dev.vault.core.VaultIdentity
import dev.vault.core.VaultRepository
import java.io.File
import java.io.FileOutputStream
import java.security.GeneralSecurityException
import java.security.SecureRandom
import uniffi.vault_ffi.FfiException
import uniffi.vault_ffi.HighWaterStore

/**
 * One vault's files, all under `noBackupFilesDir/vaults/<id>/` (app-private,
 * never backed up, never replicated):
 *
 * - `container/` — the vault-container directory (encrypted by itself);
 * - `crk.wrapped` — the CRK wrapped with K_auth;
 * - `identity.sealed`, `high-water.sealed`, `manifest-key.sealed`,
 *   `replicas.sealed` — sealed with K_bg.
 *
 * `<id>` is random (32 hex characters): a restore only learns the vault's
 * identity after the container is written, and no browser-supplied text
 * becomes a path. Plaintext never touches the disk.
 */
class VaultDir(val root: File) {
    val id: String get() = root.name
    val container: File get() = File(root, "container")
    val wrappedCrk: File get() = File(root, "crk.wrapped")

    fun sealed(kind: String): File = File(root, "$kind.sealed")

    /**
     * Whether the vault is complete. The identity file is written last: after
     * the recovery key was confirmed (create) or the CRK wrapped (restore).
     * Anything else is removed by [VaultStorage.sweepIncomplete].
     */
    val isComplete: Boolean get() = wrappedCrk.isFile && sealed(KIND_IDENTITY).isFile

    companion object {
        const val KIND_IDENTITY = "identity"
        const val KIND_HIGH_WATER = "high-water"
        const val KIND_MANIFEST_KEY = "manifest-key"
        const val KIND_REPLICAS = "replicas"
    }
}

/** Writes [bytes] to [file] atomically: a temporary file, fsync, then rename. */
fun writeAtomically(file: File, bytes: ByteArray) {
    val tmp = File(file.parentFile, ".${file.name}.tmp")
    try {
        FileOutputStream(tmp).use { out ->
            out.write(bytes)
            out.fd.sync()
        }
        if (!tmp.renameTo(file)) throw java.io.IOException("cannot replace ${file.name}")
    } finally {
        tmp.delete()
    }
}

/** The vaults directory and its sealed files. */
class VaultStorage(context: Context, private val keys: VaultKeys) : VaultRepository {
    val vaultsRoot: File = File(context.noBackupFilesDir, "vaults")
    private val random = SecureRandom()

    /** A new, empty vault directory with a fresh random id. */
    fun newVaultDir(): VaultDir {
        vaultsRoot.mkdirs()
        repeat(8) {
            val id = ByteArray(16).also(random::nextBytes).joinToString("") { "%02x".format(it) }
            val dir = File(vaultsRoot, id)
            if (dir.mkdir()) return VaultDir(dir)
        }
        throw java.io.IOException("cannot create a vault directory (storage full?)")
    }

    /**
     * Removes what an interrupted create or restore left: incomplete vault
     * directories, and Keystore keys without a vault directory. Run once at
     * process start, before any create or restore can be in flight.
     */
    fun sweepIncomplete() {
        val dirs = (vaultsRoot.listFiles() ?: emptyArray()).filter { it.isDirectory && ID.matches(it.name) }
        for (d in dirs.map(::VaultDir).filterNot { it.isComplete }) runCatching { delete(d) }
        val present = dirs.filter { it.exists() }.map { it.name }.toSet()
        for (id in keys.vaultIdsWithKeys() - present) keys.deleteKeys(id)
    }

    /** A new random replica id (16 hex characters). */
    fun newReplicaId(): String = ByteArray(8).also(random::nextBytes).joinToString("") { "%02x".format(it) }

    /** Complete vaults on this phone. */
    fun vaultDirs(): List<VaultDir> =
        (vaultsRoot.listFiles() ?: emptyArray())
            .filter { it.isDirectory && ID.matches(it.name) }
            .map(::VaultDir)
            .filter { it.isComplete }

    fun identityOf(dir: VaultDir): VaultIdentity? = try {
        val text = keys.openBackground(dir.id, VaultDir.KIND_IDENTITY, dir.sealed(VaultDir.KIND_IDENTITY).readBytes())
            .toString(Charsets.UTF_8)
        val nl = text.indexOf('\n')
        if (nl < 0) null else (VaultIdentity.of(text.substring(0, nl), text.substring(nl + 1)) as? Checked.Valid)?.value
    } catch (e: GeneralSecurityException) {
        null
    } catch (e: java.io.IOException) {
        null
    }

    fun writeIdentity(dir: VaultDir, identity: VaultIdentity) =
        writeSealed(dir, VaultDir.KIND_IDENTITY, "${identity.appId}\n${identity.origin}".toByteArray(Charsets.UTF_8))

    fun dirOf(identity: VaultIdentity): VaultDir? = vaultDirs().firstOrNull { identityOf(it) == identity }

    override fun list(): List<VaultIdentity> = vaultDirs().mapNotNull(::identityOf)

    override fun exists(identity: VaultIdentity): Boolean = dirOf(identity) != null

    /** Seals [plaintext] under K_bg into the vault's [kind] file, then wipes it. */
    fun writeSealed(dir: VaultDir, kind: String, plaintext: ByteArray) {
        try {
            writeAtomically(dir.sealed(kind), keys.sealBackground(dir.id, kind, plaintext))
        } finally {
            plaintext.fill(0)
        }
    }

    /** The plaintext of a sealed file, or null if it doesn't exist. Wipe the result. */
    fun readSealed(dir: VaultDir, kind: String): ByteArray? {
        val f = dir.sealed(kind)
        if (!f.isFile) return null
        return keys.openBackground(dir.id, kind, f.readBytes())
    }

    /**
     * Deletes a vault's files and keys (failed create/restore, or delete). The
     * identity file goes first, so a partial failure leaves an incomplete
     * directory (swept at the next start), never a vault without keys.
     */
    fun delete(dir: VaultDir) {
        dir.sealed(VaultDir.KIND_IDENTITY).delete()
        val gone = dir.root.deleteRecursively()
        keys.deleteKeys(dir.id)
        if (!gone) throw java.io.IOException("could not delete every vault file")
    }

    /** The high-water store vault-ffi calls for [dir]. */
    fun highWaterStore(dir: VaultDir): HighWaterStore = SealedHighWaterStore(this, dir)

    private companion object {
        val ID = Regex("^[0-9a-f]{32}$")
    }
}

/**
 * vault-ffi's anti-rollback state, sealed under K_bg. A missing file loads as
 * `null` (only create and restore start that way); anything that fails to
 * unseal throws, so vault-ffi fails closed.
 */
private class SealedHighWaterStore(private val storage: VaultStorage, private val dir: VaultDir) : HighWaterStore {
    override fun load(): ByteArray? = try {
        storage.readSealed(dir, VaultDir.KIND_HIGH_WATER)
    } catch (e: Exception) {
        throw FfiException.Callback("high-water state unreadable: ${e.javaClass.simpleName}")
    }

    override fun save(state: ByteArray) = try {
        storage.writeSealed(dir, VaultDir.KIND_HIGH_WATER, state)
    } catch (e: Exception) {
        throw FfiException.Callback("high-water state not saved: ${e.javaClass.simpleName}")
    }
}
