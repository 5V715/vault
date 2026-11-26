// SPDX-License-Identifier: MIT
package dev.vault.android.settings

import android.content.Context
import dev.vault.android.keys.VaultKeys
import dev.vault.android.storage.VaultDir
import dev.vault.android.storage.VaultStorage
import dev.vault.android.storage.writeAtomically
import dev.vault.core.LockSettings
import dev.vault.core.ReplicationStatus
import dev.vault.core.Settings
import dev.vault.core.VaultSettings
import java.io.File

/**
 * Settings and replication status, all sealed (never in SharedPreferences or
 * in the clear):
 *
 * - app-wide lock settings in `noBackupFilesDir/app-settings.sealed`, under an
 *   app background key (`vault.app.bg`, the K_bg class: no user
 *   authentication, device unlocked);
 * - per vault, `settings.sealed` and `replication-status.sealed` under the
 *   vault's own K_bg.
 *
 * The lock settings are cached once read (the service needs them when the
 * screen goes off, when K_bg-class keys are unusable). Only a good read (or no
 * file) is cached: a read that failed (the process started while the phone
 * was locked) gives the defaults for now and is tried again next time, so a
 * stricter setting the user chose comes back once the phone is unlocked.
 */
class SettingsStore(context: Context, private val keys: VaultKeys, private val storage: VaultStorage) {
    private val appFile = File(context.noBackupFilesDir, "app-settings.sealed")

    @Volatile
    private var lock: LockSettings? = null

    fun lockSettings(): LockSettings = lock ?: (readLock()?.also { lock = it } ?: LockSettings())

    /** Null if the file can't be read now. */
    private fun readLock(): LockSettings? = try {
        if (!appFile.isFile) {
            LockSettings()
        } else {
            val plain = keys.openBackground(APP_KEY_ID, KIND_APP, appFile.readBytes())
            try {
                Settings.decodeLock(plain)
            } finally {
                plain.fill(0)
            }
        }
    } catch (e: Exception) {
        null
    }

    /** Whether the lock settings were read (not just defaulted after a failed read). */
    val lockSettingsKnown: Boolean get() = lock != null

    fun saveLockSettings(s: LockSettings) {
        keys.createBackgroundKey(APP_KEY_ID)
        writeAtomically(appFile, keys.sealBackground(APP_KEY_ID, KIND_APP, Settings.encode(s)))
        lock = s
    }

    /** Throws if the vault's K_bg can't be used (device locked, key gone). */
    fun vaultSettings(dir: VaultDir): VaultSettings =
        storage.readSealed(dir, VaultDir.KIND_SETTINGS)?.let { plain ->
            try {
                Settings.decodeVault(plain)
            } finally {
                plain.fill(0)
            }
        } ?: VaultSettings()

    fun saveVaultSettings(dir: VaultDir, s: VaultSettings) = storage.writeSealed(dir, VaultDir.KIND_SETTINGS, Settings.encode(s))

    fun status(dir: VaultDir): ReplicationStatus =
        storage.readSealed(dir, VaultDir.KIND_REPLICATION_STATUS)?.let(ReplicationStatus::decode) ?: ReplicationStatus()

    fun saveStatus(dir: VaultDir, s: ReplicationStatus) =
        storage.writeSealed(dir, VaultDir.KIND_REPLICATION_STATUS, s.encode())

    companion object {
        /** Not a vault id (those are 32 hex): never swept as an orphan key. */
        const val APP_KEY_ID = "app"
        private const val KIND_APP = "app-settings"
    }
}
