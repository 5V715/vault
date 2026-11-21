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

/** One replica's result (vault-ffi `ReplicaOutcomeFfi`). */
sealed interface ReplicaOutcome {
    data class Synced(val seq: Long) : ReplicaOutcome

    /** The replica served an older state than last written there; it was repaired. */
    data class RolledBack(val foundSeq: Long?, val lastSeenSeq: Long, val seq: Long) : ReplicaOutcome

    /** The replica holds a newer, forked or foreign state. Nothing was written. */
    data class Diverged(val replicaSeq: Long?, val primarySeq: Long) : ReplicaOutcome

    data class Adopted(val seq: Long) : ReplicaOutcome

    data class Failed(val error: String) : ReplicaOutcome
}

/** Why a whole replication call failed before any replica was tried. */
enum class ReplicationError {
    /** The local vault is older than its high-water mark (vault-ffi `Rollback`). */
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

    sealed interface Gate {
        data object Run : Gate

        data object WaitForSessionEnd : Gate

        data object WaitForDeviceUnlock : Gate

        data object WaitForNetwork : Gate

        data object WaitForWifi : Gate
    }

    sealed interface RunResult {
        /** Done; [warnings] are shown on the vault's status line. */
        data class Ok(val warnings: List<String>) : RunResult

        /** A transient failure: retry with [backoffSecs]. */
        data object Retry : RunResult

        /** Needs the user: shown prominently, no automatic retry. */
        data class NeedsAttention(val problems: List<String>) : RunResult
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

    /** Judges one run from each replica's outcome (replica id → outcome). */
    fun assess(results: List<Pair<String, ReplicaOutcome>>): RunResult {
        val diverged = results.filter { it.second is ReplicaOutcome.Diverged }
        if (diverged.isNotEmpty()) {
            return RunResult.NeedsAttention(
                diverged.map { (id, _) ->
                    "Replica $id holds a different or newer vault state; nothing was written to it. Check it before adopting."
                },
            )
        }
        if (results.any { it.second is ReplicaOutcome.Failed }) return RunResult.Retry
        return RunResult.Ok(
            results.mapNotNull { (id, o) ->
                (o as? ReplicaOutcome.RolledBack)?.let {
                    "Replica $id had been rolled back (found ${it.foundSeq ?: "nothing"}, expected ${it.lastSeenSeq}); it was repaired to ${it.seq}."
                }
            },
        )
    }

    fun assessError(error: ReplicationError): RunResult = when (error) {
        ReplicationError.Rollback -> RunResult.NeedsAttention(
            listOf("This vault's local data was rolled back below its last known state. Replication stopped; restore it from a replica."),
        )
        ReplicationError.WrongKey -> RunResult.NeedsAttention(
            listOf("The background key no longer opens this vault. Open the vault once to set up replication again."),
        )
        ReplicationError.KeyUnavailable -> RunResult.NeedsAttention(
            listOf("The background replication key is gone. Open the vault once to set up replication again."),
        )
        ReplicationError.Busy, ReplicationError.Network, ReplicationError.DeviceLocked -> RunResult.Retry
    }
}
