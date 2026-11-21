// SPDX-License-Identifier: MIT
package dev.vault.core

enum class Network { None, Metered, Unmetered }

/** What the background job sees when it starts. */
data class ReplicationContext(
    val network: Network,
    /** The vault's "Wi-Fi only" setting: replicate on unmetered networks only. */
    val wifiOnly: Boolean,
    val sessionActive: Boolean,
    /** K_bg (`setUnlockedDeviceRequired`) cannot be used while the device is locked. */
    val deviceLocked: Boolean,
)

/**
 * One replica's result, mapped from vault-ffi `ReplicaOutcomeFfi` (`Rollback`
 * there is [RolledBack] here; upload counts are left out).
 */
sealed interface ReplicaOutcome {
    data class Synced(val seq: Long) : ReplicaOutcome

    /** The replica served an older state than last written there; it was repaired. */
    data class RolledBack(val foundSeq: Long?, val lastSeenSeq: Long, val seq: Long) : ReplicaOutcome

    /** The replica holds a newer, forked or foreign state. Nothing was written. */
    data class Diverged(val replicaSeq: Long?, val primarySeq: Long) : ReplicaOutcome

    /** A diverged replica was overwritten after the user confirmed; [rollback]: it had been rolled back. */
    data class Adopted(val seq: Long, val rollback: Boolean) : ReplicaOutcome

    data class Failed(val error: String) : ReplicaOutcome
}

/** Why a whole replication call failed before any replica was tried. */
enum class ReplicationError {
    /** The local vault is older than its high-water mark (vault-ffi `RollbackDetected`). */
    Rollback,

    /** The manifest key does not open this vault. */
    WrongKey,

    /** K_bg or the sealed manifest key / credentials are gone or unreadable. */
    KeyUnavailable,

    /** A session is active or another replication runs. */
    Busy,
    Network,
    DeviceLocked,
}

/** When background replication runs and what its results mean. */
object ReplicationPolicy {
    /** The periodic job's interval, also the backoff cap. */
    const val PERIOD_SECS: Long = 6 * 3600L
    const val FIRST_BACKOFF_SECS: Long = 30L

    /**
     * Consecutive failed runs after which a retrying failure is shown to the
     * user (30 s + 1 + 2 + 4 + 8 min ≈ 15 min of retries); retries continue.
     */
    const val SURFACE_AFTER_FAILURES: Int = 5

    sealed interface Gate {
        data object Run : Gate

        data object WaitForSessionEnd : Gate

        data object WaitForDeviceUnlock : Gate

        data object WaitForNetwork : Gate

        data object WaitForWifi : Gate
    }

    /**
     * A run's verdict. [warnings] (a replica was found rolled back and repaired:
     * possible tampering) are shown whatever else happened, so they are never lost.
     */
    sealed interface RunResult {
        val warnings: List<String>

        data class Ok(override val warnings: List<String>) : RunResult

        /**
         * A transient failure: retry with [backoffSecs]. [surface]: it has failed
         * [SURFACE_AFTER_FAILURES] runs in a row, so show [problems] to the user.
         */
        data class Retry(val problems: List<String>, val surface: Boolean, override val warnings: List<String>) : RunResult

        /** Needs the user: shown prominently, no automatic retry. */
        data class NeedsAttention(val problems: List<String>, override val warnings: List<String>) : RunResult
    }

    fun gate(ctx: ReplicationContext): Gate = when {
        ctx.sessionActive -> Gate.WaitForSessionEnd
        ctx.deviceLocked -> Gate.WaitForDeviceUnlock
        ctx.network == Network.None -> Gate.WaitForNetwork
        ctx.wifiOnly && ctx.network != Network.Unmetered -> Gate.WaitForWifi
        else -> Gate.Run
    }

    /** Delay before retry number [attempt] (0-based): 30 s doubling, capped at [PERIOD_SECS]. */
    fun backoffSecs(attempt: Int): Long =
        (FIRST_BACKOFF_SECS shl attempt.coerceIn(0, 20)).coerceAtMost(PERIOD_SECS)

    /**
     * Judges one run from each replica's outcome (replica id → outcome).
     * [failedRunsBefore]: consecutive failed runs before this one.
     */
    fun assess(results: List<Pair<String, ReplicaOutcome>>, failedRunsBefore: Int = 0): RunResult {
        val warnings = results.mapNotNull { (id, o) ->
            when (o) {
                is ReplicaOutcome.RolledBack ->
                    "Replica $id had been rolled back (found ${o.foundSeq ?: "nothing"}, expected ${o.lastSeenSeq}); " +
                        "it was repaired to ${o.seq}."
                is ReplicaOutcome.Adopted ->
                    if (o.rollback) "Replica $id had been rolled back before it was overwritten." else null
                else -> null
            }
        }
        val diverged = results.filter { it.second is ReplicaOutcome.Diverged }
        if (diverged.isNotEmpty()) {
            return RunResult.NeedsAttention(
                diverged.map { (id, _) ->
                    "Replica $id holds a different or newer vault state; nothing was written to it. Check it before adopting."
                },
                warnings,
            )
        }
        val failed = results.mapNotNull { (id, o) -> (o as? ReplicaOutcome.Failed)?.let { "Replica $id: ${it.error}" } }
        if (failed.isNotEmpty()) return retry(failed, failedRunsBefore, warnings)
        return RunResult.Ok(warnings)
    }

    fun assessError(error: ReplicationError, failedRunsBefore: Int = 0): RunResult = when (error) {
        ReplicationError.Rollback -> RunResult.NeedsAttention(
            listOf("This vault's local data was rolled back below its last known state. Replication stopped; restore it from a replica."),
            emptyList(),
        )
        ReplicationError.WrongKey -> RunResult.NeedsAttention(
            listOf("The background key no longer opens this vault. Open the vault once to set up replication again."),
            emptyList(),
        )
        ReplicationError.KeyUnavailable -> RunResult.NeedsAttention(
            listOf("The background replication key is gone. Open the vault once to set up replication again."),
            emptyList(),
        )
        // Waiting for a session or the device is not a failure.
        ReplicationError.Busy, ReplicationError.DeviceLocked -> RunResult.Retry(emptyList(), surface = false, warnings = emptyList())
        ReplicationError.Network -> retry(listOf("The replicas could not be reached."), failedRunsBefore, emptyList())
    }

    private fun retry(problems: List<String>, failedRunsBefore: Int, warnings: List<String>) =
        RunResult.Retry(problems, surface = failedRunsBefore + 1 >= SURFACE_AFTER_FAILURES, warnings = warnings)
}
