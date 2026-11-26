// SPDX-License-Identifier: MIT
package dev.vault.core

import dev.vault.core.ReplicationPolicy.RunResult

/**
 * A vault's background replication state, kept between runs (the app seals
 * it under the vault's background key) and shown on the vault screen.
 */
data class ReplicationStatus(
    val lastRunMs: Long? = null,
    val lastOkMs: Long? = null,
    /** The manifest seq the replicas hold after the last good run. */
    val seq: Long? = null,
    /** Consecutive failed runs (waiting for a session or an unlock doesn't count). */
    val failedRuns: Int = 0,
    val state: State = State.Never,
    /** NeedsAttention: no automatic run until the user acts ("Replicate now"). */
    val blocked: Boolean = false,
    val problems: List<String> = emptyList(),
    /** Possible tampering (a replica found rolled back): shown until the next good run. */
    val warnings: List<String> = emptyList(),
) {
    enum class State { Never, Ok, Waiting, Retrying, NeedsAttention }

    /** Periodic, after-session and retry runs. */
    val allowsAutomaticRun: Boolean get() = !blocked

    /** "Replicate now" always may: the user has seen the problem. */
    val allowsManualRun: Boolean get() = true

    /** When the next automatic retry is due, or null (none: ok, or blocked). */
    fun retryDelaySecs(): Long? = when (state) {
        State.Retrying -> ReplicationPolicy.backoffSecs(failedRuns - 1)
        State.Waiting -> ReplicationPolicy.backoffSecs(0)
        else -> null
    }

    /** The status after a run judged [result] at [nowMs]; [seq]: the vault's seq on success. */
    fun record(result: RunResult, nowMs: Long, seq: Long? = null): ReplicationStatus = when (result) {
        is RunResult.Ok -> ReplicationStatus(
            lastRunMs = nowMs,
            lastOkMs = nowMs,
            seq = seq ?: this.seq,
            failedRuns = 0,
            state = State.Ok,
            blocked = false,
            problems = emptyList(),
            warnings = result.warnings,
        )
        is RunResult.Retry ->
            if (result.problems.isEmpty()) {
                // Waiting (session, locked device): not a failure.
                copy(state = State.Waiting, warnings = (warnings + result.warnings).distinct())
            } else {
                copy(
                    lastRunMs = nowMs,
                    failedRuns = failedRuns + 1,
                    state = State.Retrying,
                    // Shown once the retries have gone on long enough.
                    problems = if (result.surface) result.problems else emptyList(),
                    warnings = (warnings + result.warnings).distinct(),
                )
            }
        is RunResult.NeedsAttention -> copy(
            lastRunMs = nowMs,
            failedRuns = failedRuns + 1,
            state = State.NeedsAttention,
            blocked = true,
            problems = result.problems,
            warnings = (warnings + result.warnings).distinct(),
        )
    }

    fun encode(): ByteArray = KeyValues.encode(
        buildList {
            add("v" to "1")
            lastRunMs?.let { add("lastRunMs" to it.toString()) }
            lastOkMs?.let { add("lastOkMs" to it.toString()) }
            seq?.let { add("seq" to it.toString()) }
            add("failedRuns" to failedRuns.toString())
            add("state" to state.name)
            add("blocked" to blocked.toString())
            problems.forEach { add("problem" to it) }
            warnings.forEach { add("warning" to it) }
        },
    )

    companion object {
        /** Parses [encode]'s output; anything unreadable gives the empty status. */
        fun decode(bytes: ByteArray): ReplicationStatus {
            val kv = KeyValues.decode(bytes)
            val m = kv.toMap()
            return ReplicationStatus(
                lastRunMs = m["lastRunMs"]?.toLongOrNull(),
                lastOkMs = m["lastOkMs"]?.toLongOrNull(),
                seq = m["seq"]?.toLongOrNull(),
                failedRuns = m["failedRuns"]?.toIntOrNull() ?: 0,
                state = State.entries.firstOrNull { it.name == m["state"] } ?: State.Never,
                blocked = m["blocked"]?.toBooleanStrictOrNull() ?: false,
                problems = kv.filter { it.first == "problem" }.map { it.second },
                warnings = kv.filter { it.first == "warning" }.map { it.second },
            )
        }
    }
}
