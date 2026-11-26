// SPDX-License-Identifier: MIT
package dev.vault.android.replication

import android.content.Context
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.Data
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import dev.vault.android.VaultApp
import dev.vault.core.ReplicationPolicy
import java.util.concurrent.TimeUnit
import kotlin.coroutines.cancellation.CancellationException

/** What started a replication run. */
enum class Trigger { Periodic, AfterSession, Retry, Manual }

/**
 * WorkManager jobs per vault: a periodic run (every [ReplicationPolicy.PERIOD_SECS],
 * on any network, or unmetered only with "Wi-Fi only") and one-shot runs
 * (after a session, retries, waiting for a network). Job data is the random
 * vault id and the trigger only: no app id, origin, key or credential.
 */
class ReplicationScheduler(private val context: Context) {
    private val work: WorkManager get() = WorkManager.getInstance(context)

    fun schedulePeriodic(vaultId: String, wifiOnly: Boolean) {
        val req = PeriodicWorkRequestBuilder<ReplicationWorker>(ReplicationPolicy.PERIOD_SECS, TimeUnit.SECONDS)
            .setConstraints(constraints(wifiOnly))
            .setInputData(data(vaultId, Trigger.Periodic))
            .addTag(TAG)
            .build()
        work.enqueueUniquePeriodicWork(periodicName(vaultId), ExistingPeriodicWorkPolicy.UPDATE, req)
    }

    /** A run once a network (unmetered with [wifiOnly]) is there, after [delaySecs]. */
    fun once(vaultId: String, trigger: Trigger, wifiOnly: Boolean, delaySecs: Long = 0) {
        val req = OneTimeWorkRequestBuilder<ReplicationWorker>()
            .setConstraints(constraints(wifiOnly))
            .setInputData(data(vaultId, trigger))
            .setInitialDelay(delaySecs, TimeUnit.SECONDS)
            .addTag(TAG)
            .build()
        // Appended, so a run that schedules its own retry doesn't cancel itself.
        work.enqueueUniqueWork(onceName(vaultId), ExistingWorkPolicy.APPEND_OR_REPLACE, req)
    }

    fun cancel(vaultId: String) {
        work.cancelUniqueWork(periodicName(vaultId))
        work.cancelUniqueWork(onceName(vaultId))
    }

    companion object {
        const val TAG = "vault-replication"
        const val KEY_VAULT = "vault"
        const val KEY_TRIGGER = "trigger"

        fun periodicName(vaultId: String) = "replicate-periodic-$vaultId"

        fun onceName(vaultId: String) = "replicate-once-$vaultId"

        fun constraints(wifiOnly: Boolean): Constraints =
            Constraints.Builder()
                .setRequiredNetworkType(if (wifiOnly) NetworkType.UNMETERED else NetworkType.CONNECTED)
                .build()

        private fun data(vaultId: String, trigger: Trigger) =
            Data.Builder().putString(KEY_VAULT, vaultId).putString(KEY_TRIGGER, trigger.name).build()
    }
}

/** Runs [Replicator.run] for one vault; the replicator records and reschedules itself. */
class ReplicationWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    override suspend fun doWork(): Result {
        val id = inputData.getString(ReplicationScheduler.KEY_VAULT) ?: return Result.failure()
        val trigger = Trigger.entries.firstOrNull { it.name == inputData.getString(ReplicationScheduler.KEY_TRIGGER) }
            ?: Trigger.Periodic
        return try {
            (applicationContext as VaultApp).replicator.run(id, trigger)
            Result.success()
        } catch (c: CancellationException) {
            throw c
        } catch (e: Exception) {
            // Recorded by the replicator where it could be; never logged (no secrets in logs).
            Result.success()
        }
    }
}
