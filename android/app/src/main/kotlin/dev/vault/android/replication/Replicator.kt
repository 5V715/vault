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
import dev.vault.android.vault.VaultLocks
import dev.vault.android.vault.VaultManager
import dev.vault.core.Checked
import dev.vault.core.JobKind
import dev.vault.core.Network
import dev.vault.core.ReplicaAddress
import dev.vault.core.ReplicaOutcome
import dev.vault.core.ReplicationContext
import dev.vault.core.ReplicationError
import dev.vault.core.ReplicationJobs
import dev.vault.core.ReplicationPolicy
import dev.vault.core.ReplicationPolicy.FollowUp
import dev.vault.core.ReplicationPolicy.Gate
import dev.vault.core.ReplicationStatus
import dev.vault.core.Trigger
import dev.vault.core.VaultSettings
import java.util.concurrent.ConcurrentHashMap
import kotlin.coroutines.cancellation.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Deferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import uniffi.vault_ffi.FfiException
import uniffi.vault_ffi.NamedReplica
import uniffi.vault_ffi.ReplicaOutcomeFfi
import uniffi.vault_ffi.ReplicaTarget

/** The active network as replication sees it. */
fun currentNetwork(context: Context): Network {
    val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
    val caps = cm.getNetworkCapabilities(cm.activeNetwork) ?: return Network.None
    if (!caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)) return Network.None
    return if (caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)) Network.Unmetered else Network.Metered
}

/** A replica target the app refuses to save ([reason] is shown to the user). */
class InvalidReplicaException(val reason: String) : IllegalArgumentException(reason)

/** A replica as the vault screen shows it: where it is, never its credentials. */
data class ReplicaInfo(val id: String, val description: String)

/** The replica list's text: no credentials, no user info in a URL. */
fun ReplicaTarget.describe(): String = when (this) {
    is ReplicaTarget.S3 -> "S3 · ${ReplicaAddress.redacted(endpoint)} · $bucket${if (prefix.isNotEmpty()) "/$prefix" else ""}"
    is ReplicaTarget.WebDav -> "WebDAV · ${ReplicaAddress.redacted(url)}"
    is ReplicaTarget.Local -> "Local · $path"
}

/** A target's address checked as vault-ffi will (null if fine). */
fun ReplicaTarget.addressProblem(): String? {
    val url = when (this) {
        is ReplicaTarget.S3 -> endpoint
        is ReplicaTarget.WebDav -> url
        is ReplicaTarget.Local -> return null
    }
    return (ReplicaAddress.check(url) as? Checked.Invalid)?.reason
}

/**
 * Background replication of one vault (spec, option B): the sealed manifest
 * key and replica credentials, both under K_bg, and a locked handle. It never
 * uses the CRK or K_auth, so it runs without the user and while the vault is
 * locked; it needs the device unlocked (K_bg) and no session.
 *
 * Every run is judged by [ReplicationPolicy] and recorded in the vault's
 * sealed [ReplicationStatus]; its follow-up (a retry) comes from the policy
 * too ([followUp]). A rollback (or anything else that needs the user) blocks
 * automatic runs until "Replicate now".
 */
class Replicator(
    private val storage: VaultStorage,
    private val manager: VaultManager,
    private val settings: SettingsStore,
    private val locks: VaultLocks,
    private val jobs: ReplicationJobs,
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
        jobs: ReplicationJobs,
        sessionActive: suspend (String) -> Boolean,
    ) : this(
        storage,
        manager,
        settings,
        locks,
        jobs,
        sessionActive,
        network = { currentNetwork(context) },
        deviceLocked = { (context.getSystemService(Context.KEYGUARD_SERVICE) as KeyguardManager).isDeviceLocked },
    )

    /** Runs that outlive the screen that started them ("Replicate now", session ends). */
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    /** Vault id → the run holding its handle (a pairing cancels it, [yieldTo]). */
    private val inFlight = ConcurrentHashMap<String, Job>()

    /** Vault id → runs in a row that found the phone locked (the wait grows). */
    private val lockedWaits = ConcurrentHashMap<String, Int>()

    /** How a run went (the details are in the recorded status). */
    sealed interface Report {
        val followUp: FollowUp

        /** Nothing to do: no such vault, no replicas, or blocked until the user acts. */
        data class Skipped(val reason: String) : Report {
            override val followUp: FollowUp get() = FollowUp.None
        }

        /** Not run now: [gate] says why. */
        data class Waiting(val gate: Gate, override val followUp: FollowUp) : Report

        data class Done(val status: ReplicationStatus, override val followUp: FollowUp) : Report
    }

    /** Schedules what [report] asks for (the job service calls it after its job finished). */
    fun followUp(vaultId: String, report: Report) {
        val f = report.followUp
        if (f is FollowUp.Retry) jobs.scheduleOnce(vaultId, JobKind.Retry, f.wifiOnly, f.delaySecs)
    }

    /** [run] and its follow-up, for callers that are not a job. */
    suspend fun runAndFollowUp(vaultId: String, trigger: Trigger): Report =
        run(vaultId, trigger).also { followUp(vaultId, it) }

    /** "Replicate now": in the replicator's scope, so leaving the screen doesn't cancel it. */
    fun replicateNow(vaultId: String): Deferred<Report> = scope.async { runAndFollowUp(vaultId, Trigger.Manual) }

    suspend fun run(vaultId: String, trigger: Trigger): Report = withContext(Dispatchers.IO) {
        val dir = storage.dirById(vaultId) ?: return@withContext Report.Skipped("no such vault")
        val pre = ReplicationContext(network(), wifiOnly = false, sessionActive = sessionActive(vaultId), deviceLocked = deviceLocked())
        val first = ReplicationPolicy.gate(pre)
        // K_bg is unusable while the device is locked: nothing can be read yet.
        if (first == Gate.WaitForDeviceUnlock || first == Gate.WaitForSessionEnd) {
            return@withContext waiting(vaultId, first, wifiOnly = false)
        }
        lockedWaits.remove(vaultId)
        val wifiOnly = vaultSettings(dir).wifiOnly
        val ctx = pre.copy(wifiOnly = wifiOnly && trigger != Trigger.Manual)
        val gate = ReplicationPolicy.gate(ctx)
        if (gate != Gate.Run) return@withContext waiting(vaultId, gate, ctx.wifiOnly)
        if (!hasReplicas(dir)) return@withContext Report.Skipped("no replicas")
        val lock = locks.of(vaultId)
        if (withTimeoutOrNull(LOCK_WAIT_MS) { lock.lock() } == null) {
            // A pairing (or another writer) holds the vault: try again shortly.
            return@withContext waiting(vaultId, Gate.WaitForSessionEnd, ctx.wifiOnly)
        }
        currentCoroutineContext()[Job]?.let { inFlight[vaultId] = it }
        try {
            // Read under the lock: another run may just have recorded (and blocked).
            val status = readStatus(dir)
            if (trigger != Trigger.Manual && !status.allowsAutomaticRun) {
                Report.Skipped("needs attention")
            } else {
                replicateLocked(dir, status, ctx.wifiOnly)
            }
        } finally {
            inFlight.remove(vaultId)
            lock.unlock()
        }
    }

    /**
     * A pairing of [vaultId] starts: an interactive action wins over a
     * background upload, which resumes on its next run. Cancels a run that
     * holds the vault (and waits until it let go) and schedules a retry.
     */
    suspend fun yieldTo(vaultId: String) {
        val running = inFlight[vaultId] ?: return
        running.cancel()
        running.join()
        val dir = storage.dirById(vaultId) ?: return
        jobs.scheduleOnce(vaultId, JobKind.Retry, vaultSettings(dir).wifiOnly, ReplicationPolicy.backoffSecs(0))
    }

    private fun waiting(vaultId: String, gate: Gate, wifiOnly: Boolean): Report {
        val n = if (gate == Gate.WaitForDeviceUnlock) (lockedWaits.merge(vaultId, 1, Int::plus) ?: 1) - 1 else 0
        return Report.Waiting(gate, ReplicationPolicy.afterGate(gate, wifiOnly, n))
    }

    /** An unreadable settings file fails closed: Wi-Fi only. */
    private fun vaultSettings(dir: VaultDir): VaultSettings = try {
        settings.vaultSettings(dir)
    } catch (e: Exception) {
        VaultSettings(wifiOnly = true)
    }

    /** An unreadable status is not a problem of the replicas: start from empty. */
    private fun readStatus(dir: VaultDir): ReplicationStatus = try {
        settings.status(dir)
    } catch (e: Exception) {
        ReplicationStatus()
    }

    private fun hasReplicas(dir: VaultDir): Boolean = dir.sealed(VaultDir.KIND_REPLICAS).isFile

    /** Decodes the sealed replica config and wipes its plaintext. */
    private fun readReplicas(dir: VaultDir): List<NamedReplica> {
        val plain = storage.readSealed(dir, VaultDir.KIND_REPLICAS) ?: return emptyList()
        return try {
            ReplicaConfig.decode(plain)
        } finally {
            plain.fill(0)
        }
    }

    private fun writeReplicas(dir: VaultDir, replicas: List<NamedReplica>) =
        storage.writeSealed(dir, VaultDir.KIND_REPLICAS, ReplicaConfig.encode(replicas))

    private suspend fun replicateLocked(dir: VaultDir, status: ReplicationStatus, wifiOnly: Boolean): Report {
        val targets = try {
            readReplicas(dir)
        } catch (e: Exception) {
            return record(dir, status, ReplicationPolicy.assessError(keyError(), status.failedRuns), wifiOnly)
        }
        if (targets.isEmpty()) return Report.Skipped("no replicas")
        val manifestKey = try {
            storage.readSealed(dir, VaultDir.KIND_MANIFEST_KEY)
        } catch (e: Exception) {
            null
        } ?: return record(dir, status, ReplicationPolicy.assessError(keyError(), status.failedRuns), wifiOnly)
        val results = try {
            manager.open(dir).use { handle -> handle.replicate(manifestKey, targets) }
        } catch (c: CancellationException) {
            throw c
        } catch (e: Exception) {
            val error = when (e) {
                is FfiException.RollbackDetected, is FfiException.Rollback -> ReplicationError.Rollback
                is FfiException.WrongKey -> ReplicationError.WrongKey
                is FfiException.HighWaterMissing -> ReplicationError.KeyUnavailable
                is FfiException.Busy -> ReplicationError.Busy
                is FfiException.InvalidInput, is FfiException.Unsupported, is FfiException.Tls -> ReplicationError.BadTarget
                else -> ReplicationError.Network
            }
            return record(dir, status, ReplicationPolicy.assessError(error, status.failedRuns), wifiOnly)
        } finally {
            manifestKey.fill(0)
        }
        // Problems name the replica as the user knows it, not by its random id.
        val labels = targets.associate { it.id to it.target.describe() }
        val outcomes = results.map { (labels[it.id] ?: it.id) to it.outcome.toCore() }
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

    /** A sealed file that can't be read: the phone locked meanwhile, or K_bg is gone. */
    private fun keyError() = if (deviceLocked()) ReplicationError.DeviceLocked else ReplicationError.KeyUnavailable

    private fun record(
        dir: VaultDir,
        prev: ReplicationStatus,
        result: ReplicationPolicy.RunResult,
        wifiOnly: Boolean,
        seq: Long? = null,
    ): Report {
        val next = prev.record(result, clock(), seq)
        try {
            settings.saveStatus(dir, next)
        } catch (e: Exception) {
            // K_bg unusable: the status stays as it was; the result is still returned.
        }
        return Report.Done(next, ReplicationPolicy.afterRun(next, wifiOnly))
    }

    /** Adds a replica target (sealed with the others under K_bg) and schedules the periodic run. */
    suspend fun addReplica(dir: VaultDir, target: ReplicaTarget) = withContext(Dispatchers.IO) {
        target.addressProblem()?.let { throw InvalidReplicaException(it) }
        writeReplicas(dir, readReplicas(dir) + NamedReplica(storage.newReplicaId(), target))
        jobs.schedulePeriodic(dir.id, vaultSettings(dir).wifiOnly)
    }

    /** Replaces replica [id]'s target, keeping its id (and so its anti-rollback marks). */
    suspend fun updateReplica(dir: VaultDir, id: String, target: ReplicaTarget) = withContext(Dispatchers.IO) {
        target.addressProblem()?.let { throw InvalidReplicaException(it) }
        writeReplicas(dir, readReplicas(dir).map { if (it.id == id) NamedReplica(id, target) else it })
    }

    /** Removes the replica [id] (the replica's data stays where it is). */
    suspend fun removeReplica(dir: VaultDir, id: String) = withContext(Dispatchers.IO) {
        val rest = readReplicas(dir).filterNot { it.id == id }
        writeReplicas(dir, rest)
        if (rest.isEmpty()) jobs.cancelAll(dir.id)
    }

    /** Replica [id] for the editor, its secret left out (it is typed again). */
    suspend fun replicaForEdit(dir: VaultDir, id: String): ReplicaTarget? = withContext(Dispatchers.IO) {
        when (val t = readReplicas(dir).firstOrNull { it.id == id }?.target) {
            is ReplicaTarget.S3 -> ReplicaTarget.S3(t.endpoint, t.region, t.bucket, t.prefix, t.accessKeyId, "", t.pathStyle)
            is ReplicaTarget.WebDav -> ReplicaTarget.WebDav(t.url, t.user, "")
            else -> t
        }
    }

    /** The vault's replicas as shown (no credentials leave this class). */
    suspend fun replicaInfos(dir: VaultDir): List<ReplicaInfo> = withContext(Dispatchers.IO) {
        readReplicas(dir).map { ReplicaInfo(it.id, it.target.describe()) }
    }

    /** Wi-Fi only changed: the periodic run's constraint follows. */
    suspend fun setWifiOnly(dir: VaultDir, wifiOnly: Boolean) = withContext(Dispatchers.IO) {
        settings.saveVaultSettings(dir, VaultSettings(wifiOnly = wifiOnly))
        if (hasReplicas(dir)) jobs.schedulePeriodic(dir.id, wifiOnly)
    }

    /**
     * Every vault with replicas has its periodic run, and one overdue since
     * its last success (phone off, standby limits) gets a run now. Call off
     * the main thread when the vault list loads.
     */
    fun ensureScheduled(dirs: List<VaultDir>) {
        val now = clock()
        for (dir in dirs) {
            if (!hasReplicas(dir)) continue
            val wifiOnly = vaultSettings(dir).wifiOnly
            jobs.schedulePeriodic(dir.id, wifiOnly)
            val overdue = readStatus(dir).lastOkMs?.let { now - it > ReplicationPolicy.PERIOD_SECS * 1000 } ?: true
            if (overdue) jobs.scheduleOnce(dir.id, JobKind.Retry, wifiOnly, 0)
        }
    }

    /** The vault was deleted: no more runs for it. */
    fun forget(vaultId: String) {
        jobs.cancelAll(vaultId)
        lockedWaits.remove(vaultId)
    }

    /** A session of the vault ended: replicate what it committed (off the main thread). */
    fun afterSession(vaultId: String) {
        scope.launch {
            val dir = storage.dirById(vaultId) ?: return@launch
            jobs.scheduleOnce(vaultId, JobKind.AfterSession, vaultSettings(dir).wifiOnly, 0)
        }
    }

    private companion object {
        /** How long a run waits for a pairing that is just closing its handle. */
        const val LOCK_WAIT_MS = 30_000L
    }
}

internal fun ReplicaOutcomeFfi.toCore(): ReplicaOutcome = when (this) {
    is ReplicaOutcomeFfi.Synced -> ReplicaOutcome.Synced(seq.toLong())
    is ReplicaOutcomeFfi.Rollback -> ReplicaOutcome.RolledBack(foundSeq?.toLong(), lastSeenSeq.toLong(), seq.toLong())
    is ReplicaOutcomeFfi.Diverged -> ReplicaOutcome.Diverged(replicaSeq?.toLong(), primarySeq.toLong())
    is ReplicaOutcomeFfi.Adopted -> ReplicaOutcome.Adopted(seq.toLong(), rollback)
    is ReplicaOutcomeFfi.Failed -> ReplicaOutcome.Failed(error)
}
