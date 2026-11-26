// SPDX-License-Identifier: MIT
package dev.vault.android.replication

import android.app.KeyguardManager
import android.content.Context
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import dev.vault.android.settings.SettingsStore
import dev.vault.android.storage.ReplicaConfig
import dev.vault.android.storage.VaultDir
import dev.vault.android.storage.VaultStorage
import dev.vault.android.vault.VaultManager
import dev.vault.core.Network
import dev.vault.core.ReplicaOutcome
import dev.vault.core.ReplicationContext
import dev.vault.core.ReplicationError
import dev.vault.core.ReplicationPolicy
import dev.vault.core.ReplicationPolicy.Gate
import dev.vault.core.ReplicationStatus
import kotlin.coroutines.cancellation.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import uniffi.vault_ffi.FfiException
import uniffi.vault_ffi.NamedReplica
import uniffi.vault_ffi.ReplicaOutcomeFfi
import uniffi.vault_ffi.ReplicaTarget

/**
 * One mutex per vault: its container takes one handle at a time. A pairing
 * holds it from opening its handle until the handle is closed; a replication
 * run holds it while it replicates.
 */
class VaultLocks {
    private val locks = HashMap<String, Mutex>()

    @Synchronized
    fun of(vaultId: String): Mutex = locks.getOrPut(vaultId) { Mutex() }
}

/** The active network as replication sees it. */
fun currentNetwork(context: Context): Network {
    val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
    val caps = cm.getNetworkCapabilities(cm.activeNetwork) ?: return Network.None
    if (!caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)) return Network.None
    return if (caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)) Network.Unmetered else Network.Metered
}

/**
 * Background replication of one vault (spec, option B): the sealed manifest
 * key and replica credentials, both under K_bg, and a locked handle. It never
 * uses the CRK or K_auth, so it runs without the user and while the vault is
 * locked; it needs the device unlocked (K_bg) and no session.
 *
 * Every run is judged by [ReplicationPolicy] and recorded in the vault's
 * sealed [ReplicationStatus]. A rollback (or anything else that needs the
 * user) blocks automatic runs until "Replicate now"; transient failures are
 * retried with backoff.
 */
class Replicator(
    private val storage: VaultStorage,
    private val manager: VaultManager,
    private val settings: SettingsStore,
    private val locks: VaultLocks,
    private val scheduler: ReplicationScheduler,
    /** Whether the pairing coordinator holds the vault (pairing or serving). */
    private val sessionActive: suspend (String) -> Boolean,
    private val network: () -> Network,
    private val deviceLocked: () -> Boolean,
    private val clock: () -> Long = System::currentTimeMillis,
) {
    constructor(
        context: Context,
        storage: VaultStorage,
        manager: VaultManager,
        settings: SettingsStore,
        locks: VaultLocks,
        scheduler: ReplicationScheduler,
        sessionActive: suspend (String) -> Boolean,
    ) : this(
        storage,
        manager,
        settings,
        locks,
        scheduler,
        sessionActive,
        network = { currentNetwork(context) },
        deviceLocked = { (context.getSystemService(Context.KEYGUARD_SERVICE) as KeyguardManager).isDeviceLocked },
    )

    /** How a run went (the details are in the recorded status). */
    sealed interface Report {
        /** Nothing to do: no such vault, no replicas, or blocked until the user acts. */
        data class Skipped(val reason: String) : Report

        /** Not run now: [gate] says why; a later run is scheduled where one is due. */
        data class Waiting(val gate: Gate) : Report

        data class Done(val status: ReplicationStatus) : Report
    }

    suspend fun run(vaultId: String, trigger: Trigger): Report = withContext(Dispatchers.IO) {
        val dir = storage.dirById(vaultId) ?: return@withContext Report.Skipped("no such vault")
        // K_bg is unusable while the device is locked: nothing can be read.
        if (deviceLocked()) {
            // Not a failure; checked again later (the periodic run also comes).
            scheduler.once(vaultId, Trigger.Retry, wifiOnly = false, delaySecs = UNLOCK_WAIT_SECS)
            return@withContext Report.Waiting(Gate.WaitForDeviceUnlock)
        }
        val status = try {
            settings.status(dir)
        } catch (e: Exception) {
            return@withContext record(dir, ReplicationStatus(), ReplicationPolicy.assessError(ReplicationError.KeyUnavailable))
        }
        if (trigger != Trigger.Manual && !status.allowsAutomaticRun) {
            return@withContext Report.Skipped("needs attention")
        }
        val targets = try {
            storage.readSealed(dir, VaultDir.KIND_REPLICAS)?.let { ReplicaConfig.decode(it) } ?: emptyList()
        } catch (e: Exception) {
            return@withContext record(dir, status, ReplicationPolicy.assessError(ReplicationError.KeyUnavailable, status.failedRuns))
        }
        if (targets.isEmpty()) return@withContext Report.Skipped("no replicas")
        val wifiOnly = try {
            settings.vaultSettings(dir).wifiOnly
        } catch (e: Exception) {
            false
        }
        val ctx = ReplicationContext(
            network = network(),
            // "Replicate now" is the user's explicit request: any network.
            wifiOnly = wifiOnly && trigger != Trigger.Manual,
            sessionActive = sessionActive(vaultId),
            deviceLocked = false,
        )
        when (val gate = ReplicationPolicy.gate(ctx)) {
            Gate.Run -> Unit
            // The session's end schedules a run.
            Gate.WaitForSessionEnd -> return@withContext Report.Waiting(gate)
            Gate.WaitForNetwork, Gate.WaitForWifi -> {
                scheduler.once(vaultId, Trigger.Retry, wifiOnly = ctx.wifiOnly)
                return@withContext Report.Waiting(gate)
            }
            Gate.WaitForDeviceUnlock -> return@withContext Report.Waiting(gate)
        }
        val lock = locks.of(vaultId)
        if (withTimeoutOrNull(LOCK_WAIT_MS) { lock.lock() } == null) {
            // A pairing holds the vault; its end schedules a run.
            return@withContext Report.Waiting(Gate.WaitForSessionEnd)
        }
        try {
            replicateLocked(dir, status, targets, ctx.wifiOnly)
        } finally {
            lock.unlock()
        }
    }

    private suspend fun replicateLocked(dir: VaultDir, status: ReplicationStatus, targets: List<NamedReplica>, wifiOnly: Boolean): Report {
        val manifestKey = try {
            storage.readSealed(dir, VaultDir.KIND_MANIFEST_KEY)
        } catch (e: Exception) {
            null
        } ?: return record(dir, status, ReplicationPolicy.assessError(ReplicationError.KeyUnavailable, status.failedRuns))
        val results = try {
            manager.open(dir).use { handle -> handle.replicate(manifestKey, targets) }
        } catch (c: CancellationException) {
            throw c
        } catch (e: FfiException) {
            val error = when (e) {
                is FfiException.RollbackDetected, is FfiException.Rollback -> ReplicationError.Rollback
                is FfiException.WrongKey -> ReplicationError.WrongKey
                is FfiException.HighWaterMissing -> ReplicationError.KeyUnavailable
                is FfiException.Busy -> ReplicationError.Busy
                else -> ReplicationError.Network
            }
            val r = record(dir, status, ReplicationPolicy.assessError(error, status.failedRuns), wifiOnly)
            return r
        } finally {
            manifestKey.fill(0)
        }
        val outcomes = results.map { it.id to it.outcome.toCore() }
        val seq = outcomes.mapNotNull { (_, o) ->
            when (o) {
                is ReplicaOutcome.Synced -> o.seq
                is ReplicaOutcome.RolledBack -> o.seq
                is ReplicaOutcome.Adopted -> o.seq
                else -> null
            }
        }.maxOrNull()
        return record(dir, status, ReplicationPolicy.assess(outcomes, status.failedRuns), wifiOnly, seq)
    }

    /** Records [result] and schedules its retry, if one is due. */
    private fun record(
        dir: VaultDir,
        prev: ReplicationStatus,
        result: ReplicationPolicy.RunResult,
        wifiOnly: Boolean = false,
        seq: Long? = null,
    ): Report {
        val next = prev.record(result, clock(), seq)
        try {
            settings.saveStatus(dir, next)
        } catch (e: Exception) {
            // K_bg unusable: the status stays as it was; the result is still returned.
        }
        if (result is ReplicationPolicy.RunResult.Retry) {
            next.retryDelaySecs()?.let { scheduler.once(dir.id, Trigger.Retry, wifiOnly, it) }
        }
        return Report.Done(next)
    }

    /** Adds a replica target (sealed with the others under K_bg) and schedules the periodic run. */
    suspend fun addReplica(dir: VaultDir, target: ReplicaTarget) = withContext(Dispatchers.IO) {
        val existing = storage.readSealed(dir, VaultDir.KIND_REPLICAS)?.let { ReplicaConfig.decode(it) } ?: emptyList()
        val all = existing + NamedReplica(storage.newReplicaId(), target)
        storage.writeSealed(dir, VaultDir.KIND_REPLICAS, ReplicaConfig.encode(all))
        scheduler.schedulePeriodic(dir.id, settings.vaultSettings(dir).wifiOnly)
    }

    /** Removes the replica [id] (the replica's data stays where it is). */
    suspend fun removeReplica(dir: VaultDir, id: String) = withContext(Dispatchers.IO) {
        val rest = (storage.readSealed(dir, VaultDir.KIND_REPLICAS)?.let { ReplicaConfig.decode(it) } ?: emptyList())
            .filterNot { it.id == id }
        storage.writeSealed(dir, VaultDir.KIND_REPLICAS, ReplicaConfig.encode(rest))
        if (rest.isEmpty()) scheduler.cancel(dir.id)
    }

    /** The vault's replicas (with credentials: keep them in memory only as long as shown). */
    suspend fun replicas(dir: VaultDir): List<NamedReplica> = withContext(Dispatchers.IO) {
        storage.readSealed(dir, VaultDir.KIND_REPLICAS)?.let { ReplicaConfig.decode(it) } ?: emptyList()
    }

    /** Wi-Fi only changed: the periodic run's constraint follows. */
    suspend fun setWifiOnly(dir: VaultDir, wifiOnly: Boolean) = withContext(Dispatchers.IO) {
        settings.saveVaultSettings(dir, settings.vaultSettings(dir).copy(wifiOnly = wifiOnly))
        if (replicas(dir).isNotEmpty()) scheduler.schedulePeriodic(dir.id, wifiOnly)
    }

    /**
     * Every vault with replicas has its periodic run (after a restore, or if
     * WorkManager lost it). Call off the main thread when the list loads.
     */
    fun ensureScheduled(dirs: List<VaultDir>) {
        for (dir in dirs) {
            if (!dir.sealed(VaultDir.KIND_REPLICAS).isFile) continue
            val wifiOnly = try {
                settings.vaultSettings(dir).wifiOnly
            } catch (e: Exception) {
                false
            }
            scheduler.schedulePeriodic(dir.id, wifiOnly)
        }
    }

    /** The vault was deleted: no more runs for it. */
    fun forget(vaultId: String) = scheduler.cancel(vaultId)

    /** A session of the vault ended: replicate what it committed. */
    fun afterSession(vaultId: String) {
        val dir = storage.dirById(vaultId) ?: return
        val wifiOnly = try {
            settings.vaultSettings(dir).wifiOnly
        } catch (e: Exception) {
            false
        }
        scheduler.once(vaultId, Trigger.AfterSession, wifiOnly)
    }

    private companion object {
        /** How long a run waits for a pairing that is just closing its handle. */
        const val LOCK_WAIT_MS = 30_000L

        /** A run that found the device locked looks again after this long. */
        const val UNLOCK_WAIT_SECS = 15 * 60L
    }
}

internal fun ReplicaOutcomeFfi.toCore(): ReplicaOutcome = when (this) {
    is ReplicaOutcomeFfi.Synced -> ReplicaOutcome.Synced(seq.toLong())
    is ReplicaOutcomeFfi.Rollback -> ReplicaOutcome.RolledBack(foundSeq?.toLong(), lastSeenSeq.toLong(), seq.toLong())
    is ReplicaOutcomeFfi.Diverged -> ReplicaOutcome.Diverged(replicaSeq?.toLong(), primarySeq.toLong())
    is ReplicaOutcomeFfi.Adopted -> ReplicaOutcome.Adopted(seq.toLong(), rollback)
    is ReplicaOutcomeFfi.Failed -> ReplicaOutcome.Failed(error)
}
