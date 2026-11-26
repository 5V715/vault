// SPDX-License-Identifier: MIT
package dev.vault.core

import dev.vault.core.ReplicationPolicy.RunResult
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ReplicationStatusTest {
    @Test
    fun ok_resets_failures_and_records_the_time() {
        val s = ReplicationStatus().record(RunResult.Retry(listOf("x"), false, emptyList()), nowMs = 1000)
        assertEquals(1, s.failedRuns)
        assertEquals(ReplicationStatus.State.Retrying, s.state)
        assertEquals(ReplicationPolicy.backoffSecs(0), s.retryDelaySecs())
        val ok = s.record(RunResult.Ok(listOf("warn")), nowMs = 2000, seq = 7)
        assertEquals(0, ok.failedRuns)
        assertEquals(2000L, ok.lastOkMs)
        assertEquals(2000L, ok.lastRunMs)
        assertEquals(7L, ok.seq)
        assertEquals(listOf("warn"), ok.warnings)
        assertNull(ok.retryDelaySecs())
        assertFalse(ok.blocked)
    }

    @Test
    fun retries_back_off_and_waiting_is_not_a_failure() {
        var s = ReplicationStatus()
        for (i in 0 until 3) s = s.record(RunResult.Retry(listOf("down"), false, emptyList()), nowMs = i.toLong())
        assertEquals(3, s.failedRuns)
        assertEquals(ReplicationPolicy.backoffSecs(2), s.retryDelaySecs())
        // Busy / device locked: Retry with no problems; the count stays.
        val waiting = s.record(RunResult.Retry(emptyList(), false, emptyList()), nowMs = 10)
        assertEquals(3, waiting.failedRuns)
        assertEquals(ReplicationStatus.State.Waiting, waiting.state)
    }

    @Test
    fun needs_attention_blocks_automatic_runs_until_the_user_acts() {
        val rb = ReplicationPolicy.assessError(ReplicationError.Rollback)
        val s = ReplicationStatus().record(rb, nowMs = 5)
        assertTrue(s.blocked)
        assertEquals(ReplicationStatus.State.NeedsAttention, s.state)
        assertNull("never retried", s.retryDelaySecs())
        assertFalse(s.allowsAutomaticRun)
        assertTrue(s.problems.single().contains("rolled back"))
        // "Replicate now" is the user acting: an Ok unblocks.
        assertFalse(s.record(RunResult.Ok(emptyList()), nowMs = 6).blocked)
    }

    @Test
    fun status_round_trip() {
        val s = ReplicationStatus(
            lastRunMs = 11,
            lastOkMs = 7,
            seq = 3,
            failedRuns = 2,
            state = ReplicationStatus.State.NeedsAttention,
            blocked = true,
            problems = listOf("a\nb", "back\\slash", "=", "\\n literal"),
            warnings = listOf("w"),
        )
        assertEquals(s, ReplicationStatus.decode(s.encode()))
        assertEquals(ReplicationStatus(), ReplicationStatus.decode(ByteArray(0)))
    }

    @Test
    fun stale_when_replicas_have_not_succeeded_for_a_day() {
        val day = ReplicationStatus.STALE_AFTER_MS
        val ok = ReplicationStatus().record(RunResult.Ok(emptyList()), nowMs = 1_000)
        assertFalse(ok.isStale(nowMs = 1_000 + day, hasReplicas = true))
        assertTrue(ok.isStale(nowMs = 1_001 + day, hasReplicas = true))
        assertFalse("no replicas, nothing to be stale", ok.isStale(nowMs = 10 * day, hasReplicas = false))
        assertTrue("never succeeded", ReplicationStatus().isStale(nowMs = 0, hasReplicas = true))
    }
}
