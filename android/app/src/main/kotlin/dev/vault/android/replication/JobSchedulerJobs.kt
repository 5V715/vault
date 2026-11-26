// SPDX-License-Identifier: MIT
package dev.vault.android.replication

import android.app.job.JobInfo
import android.app.job.JobParameters
import android.app.job.JobScheduler
import android.app.job.JobService
import android.content.ComponentName
import android.content.Context
import android.os.PersistableBundle
import dev.vault.android.VaultApp
import dev.vault.core.JobKind
import dev.vault.core.ReplicationJobIds
import dev.vault.core.ReplicationJobs
import dev.vault.core.ReplicationPolicy
import java.util.concurrent.ConcurrentHashMap
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch

/**
 * [ReplicationJobs] on Android's JobScheduler: no app-side job database (the
 * system keeps persisted jobs). A job's id is [ReplicationJobIds] (vault id
 * and kind) and its only extra is the random vault id.
 */
class JobSchedulerJobs(context: Context) : ReplicationJobs {
    private val context = context.applicationContext
    private val scheduler: JobScheduler get() = context.getSystemService(Context.JOB_SCHEDULER_SERVICE) as JobScheduler
    private val service = ComponentName(context, ReplicationJobService::class.java)

    private fun builder(vaultId: String, kind: JobKind, wifiOnly: Boolean): JobInfo.Builder =
        JobInfo.Builder(ReplicationJobIds.of(vaultId, kind), service)
            .setRequiredNetworkType(if (wifiOnly) JobInfo.NETWORK_TYPE_UNMETERED else JobInfo.NETWORK_TYPE_ANY)
            .setExtras(PersistableBundle().apply { putString(EXTRA_VAULT, vaultId) })
            // Survives a reboot (needs RECEIVE_BOOT_COMPLETED; no receiver of our own).
            .setPersisted(true)

    override fun schedulePeriodic(vaultId: String, wifiOnly: Boolean) {
        val period = ReplicationPolicy.PERIOD_SECS * 1000
        scheduler.schedule(builder(vaultId, JobKind.Periodic, wifiOnly).setPeriodic(period, period / 4).build())
    }

    override fun scheduleOnce(vaultId: String, kind: JobKind, wifiOnly: Boolean, delaySecs: Long) {
        require(kind != JobKind.Periodic) { "the periodic job is scheduled with schedulePeriodic" }
        scheduler.schedule(builder(vaultId, kind, wifiOnly).setMinimumLatency(delaySecs.coerceAtLeast(0) * 1000).build())
    }

    override fun cancelAll(vaultId: String) {
        for (k in JobKind.entries) scheduler.cancel(ReplicationJobIds.of(vaultId, k))
    }

    companion object {
        const val EXTRA_VAULT = "vault"
    }
}

/**
 * Runs one vault's replication when JobScheduler says so. Not exported; only
 * the system (BIND_JOB_SERVICE) binds it. The run's follow-up (a retry) is
 * scheduled after the job finished, so it never replaces the running job.
 */
class ReplicationJobService : JobService() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val running = ConcurrentHashMap<Int, Job>()

    override fun onStartJob(params: JobParameters): Boolean {
        val vaultId = params.extras.getString(JobSchedulerJobs.EXTRA_VAULT)
        val kind = ReplicationJobIds.kindOf(params.jobId)
        if (vaultId == null || kind == null) return false
        val replicator = (application as VaultApp).replicator
        running[params.jobId] = scope.launch {
            val report = try {
                replicator.run(vaultId, kind.trigger)
            } catch (e: Exception) {
                // Cancelled (a pairing took the vault, or the system stopped the
                // job; both schedule what follows) or failed: recorded by the
                // replicator where it could be, never logged.
                null
            } finally {
                running.remove(params.jobId)
            }
            jobFinished(params, false)
            report?.let { replicator.followUp(vaultId, it) }
        }
        return true
    }

    /** The system stops the job (constraint lost, time limit): it resumes on the next run. */
    override fun onStopJob(params: JobParameters): Boolean {
        running.remove(params.jobId)?.cancel()
        return true
    }

    override fun onDestroy() {
        scope.coroutineContext[Job]?.cancel()
        super.onDestroy()
    }
}
