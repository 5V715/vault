// SPDX-License-Identifier: MIT
package dev.vault.android

import android.app.Application
import java.util.concurrent.atomic.AtomicBoolean
import dev.vault.android.keys.VaultKeys
import dev.vault.android.pairing.PairingCoordinator
import dev.vault.android.replication.JobSchedulerJobs
import dev.vault.android.replication.ReplicationSessionHooks
import dev.vault.android.replication.Replicator
import dev.vault.android.settings.SettingsStore
import dev.vault.android.storage.VaultStorage
import dev.vault.android.vault.VaultLocks
import dev.vault.android.vault.VaultManager
import dev.vault.core.LockSettings

/** Process-wide objects: one VaultFactory (one Rust runtime) for the app. */
class VaultApp : Application() {
    val keys: VaultKeys by lazy { VaultKeys(this) }
    val storage: VaultStorage by lazy { VaultStorage(this, keys) }
    val settings: SettingsStore by lazy { SettingsStore(this, keys, storage) }

    /**
     * App-wide lock settings (auto-lock on screen off, idle timeout), sealed.
     * Each pairing applies the idle timeout as it is then ([VaultManager.applyLockSettings]).
     */
    val lockSettings: LockSettings get() = settings.lockSettings()

    /** One handle per container: shared by pairing, replication, re-wrap, compaction and delete. */
    val locks = VaultLocks()

    /** Background replication jobs on JobScheduler (no app-side job database). */
    val jobs: JobSchedulerJobs by lazy { JobSchedulerJobs(this) }

    /** Debug builds may trust one extra CA for the LAN showcase (release: never). */
    val manager: VaultManager by lazy {
        VaultManager(storage, keys, lockSettings, DebugCa.extraCaPem(this, keys), locks) { jobs.cancelAll(it) }
    }

    private val defaultPairing: PairingCoordinator by lazy {
        PairingCoordinator(this, manager, storage, { lockSettings }, locks, ReplicationSessionHooks { replicator })
    }

    private val defaultReplicator: Replicator by lazy {
        Replicator(this, storage, manager, settings, locks, jobs) { pairing.holds(it) }
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
