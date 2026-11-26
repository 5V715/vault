// SPDX-License-Identifier: MIT
package dev.vault.android

import android.app.Application
import androidx.work.Configuration
import java.util.concurrent.atomic.AtomicBoolean
import dev.vault.android.keys.VaultKeys
import dev.vault.android.pairing.PairingCoordinator
import dev.vault.android.pairing.SessionHooks
import dev.vault.android.replication.ReplicationScheduler
import dev.vault.android.replication.Replicator
import dev.vault.android.replication.VaultLocks
import dev.vault.android.settings.SettingsStore
import dev.vault.android.storage.VaultDir
import dev.vault.android.storage.VaultStorage
import dev.vault.android.vault.VaultManager
import dev.vault.core.LockSettings
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import uniffi.vault_ffi.VaultHandle

/**
 * Process-wide objects: one VaultFactory (one Rust runtime) for the app.
 * Also WorkManager's configuration: it starts on demand (no startup
 * initializer), so its database exists only once something was scheduled.
 */
class VaultApp : Application(), Configuration.Provider {
    val keys: VaultKeys by lazy { VaultKeys(this) }
    val storage: VaultStorage by lazy { VaultStorage(this, keys) }
    val settings: SettingsStore by lazy { SettingsStore(this, keys, storage) }

    /**
     * App-wide lock settings (auto-lock on screen off, idle timeout), sealed.
     * The idle timeout reaches vault-server through the factory, so a change
     * applies after the app restarts.
     */
    val lockSettings: LockSettings get() = settings.lockSettings()

    /** One handle per container: shared by pairing and background replication. */
    val locks = VaultLocks()
    val scheduler: ReplicationScheduler by lazy { ReplicationScheduler(this) }

    override val workManagerConfiguration: Configuration
        get() = Configuration.Builder().build()

    /** Debug builds may trust one extra CA for the LAN showcase (release: never). */
    val manager: VaultManager by lazy { VaultManager(storage, keys, lockSettings, DebugCa.extraCaPem(this, keys)) }

    /** After a session: compact if asked to, then replicate what it committed. */
    private val hooks = object : SessionHooks {
        override suspend fun beforeLock(dir: VaultDir, handle: VaultHandle) {
            val s = withContext(Dispatchers.IO) { settings.vaultSettings(dir) }
            if (!s.compactRequested) return
            handle.compact()
            withContext(Dispatchers.IO) { settings.saveVaultSettings(dir, s.copy(compactRequested = false)) }
        }

        override fun sessionEnded(vaultId: String) = replicator.afterSession(vaultId)
    }

    private val defaultPairing: PairingCoordinator by lazy {
        PairingCoordinator(this, manager, storage, { lockSettings }, locks, hooks)
    }

    private val defaultReplicator: Replicator by lazy {
        Replicator(this, storage, manager, settings, locks, scheduler) { pairing.holds(it) }
    }

    /** Background replication (debug builds: tests may substitute one, [DebugHooks]). */
    val replicator: Replicator get() = DebugHooks.replicatorOverride ?: defaultReplicator

    /**
     * Pairings and sessions (held while [dev.vault.android.pairing.SessionService]
     * runs). Debug builds let instrumented tests substitute a coordinator over
     * test keys ([DebugHooks]); in release the override is the constant null.
     */
    val pairing: PairingCoordinator get() = DebugHooks.pairingOverride ?: defaultPairing

    private val swept = AtomicBoolean(false)

    /**
     * Removes leftovers of an interrupted create or restore, once per process
     * and before the first vault list loads (so before any create or restore of
     * this process can be running). Call off the main thread.
     */
    fun sweepOnce() {
        if (swept.compareAndSet(false, true)) storage.sweepIncomplete()
    }
}
