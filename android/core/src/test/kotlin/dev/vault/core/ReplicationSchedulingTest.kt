// SPDX-License-Identifier: MIT
package dev.vault.core

import dev.vault.core.ReplicationPolicy.FollowUp
import dev.vault.core.ReplicationPolicy.Gate
import dev.vault.core.ReplicationPolicy.RunResult
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ReplicationSchedulingTest {
    private val id = "0123456789abcdef0123456789abcdef"
    private val other = "fedcba9876543210fedcba9876543210"

    @Test
    fun job_ids_carry_the_vault_and_the_kind_only() {
        val ids = JobKind.entries.map { ReplicationJobIds.of(id, it) }
        assertEquals("one id per kind", ids.size, ids.toSet().size)
        assertTrue(ids.all { it > 0 })
        for (k in JobKind.entries) assertEquals(k, ReplicationJobIds.kindOf(ReplicationJobIds.of(id, k)))
        assertNotEquals(ReplicationJobIds.of(id, JobKind.Periodic), ReplicationJobIds.of(other, JobKind.Periodic))
        assertNull(ReplicationJobIds.kindOf(-1))
        // Triggers map to kinds; "replicate now" runs in the app, never as a job.
        assertEquals(Trigger.Periodic, JobKind.Periodic.trigger)
        assertEquals(Trigger.AfterSession, JobKind.AfterSession.trigger)
        assertEquals(Trigger.Retry, JobKind.Retry.trigger)
    }

    @Test
    fun waiting_is_always_followed_up_never_in_a_hot_loop() {
        val first = ReplicationPolicy.backoffSecs(0)
        // A session (or the lock) holds the vault: try again shortly, not only at session end.
        assertEquals(FollowUp.Retry(first, wifiOnly = false), ReplicationPolicy.afterGate(Gate.WaitForSessionEnd, false, 0))
        // No network / no Wi-Fi: with the matching constraint, and a delay (no 0 s loop).
        assertEquals(FollowUp.Retry(first, wifiOnly = true), ReplicationPolicy.afterGate(Gate.WaitForWifi, true, 0))
        assertEquals(FollowUp.Retry(first, wifiOnly = false), ReplicationPolicy.afterGate(Gate.WaitForNetwork, false, 0))
        // A locked phone: 15 min, doubling, capped at the period; constraint-free (the network isn't the issue).
        val locked = (0..10).map { (ReplicationPolicy.afterGate(Gate.WaitForDeviceUnlock, true, it) as FollowUp.Retry).delaySecs }
        assertEquals(15 * 60L, locked[0])
        assertEquals(30 * 60L, locked[1])
        assertEquals(ReplicationPolicy.PERIOD_SECS, locked.last())
        assertEquals(FollowUp.None, ReplicationPolicy.afterGate(Gate.Run, false, 0))
    }

    @Test
    fun runs_are_followed_up_by_their_status() {
        val retrying = ReplicationStatus().record(RunResult.Retry(listOf("down"), false, emptyList()), nowMs = 0)
        assertEquals(FollowUp.Retry(retrying.retryDelaySecs()!!, wifiOnly = true), ReplicationPolicy.afterRun(retrying, wifiOnly = true))
        val ok = ReplicationStatus().record(RunResult.Ok(emptyList()), nowMs = 0)
        assertEquals(FollowUp.None, ReplicationPolicy.afterRun(ok, wifiOnly = false))
        val blocked = ReplicationStatus().record(ReplicationPolicy.assessError(ReplicationError.Rollback), nowMs = 0)
        assertEquals("never retried", FollowUp.None, ReplicationPolicy.afterRun(blocked, wifiOnly = false))
    }

    @Test
    fun a_refused_target_needs_the_user() {
        val r = ReplicationPolicy.assessError(ReplicationError.BadTarget)
        assertTrue(r is RunResult.NeedsAttention && r.problems.single().contains("address"))
    }

    @Test
    fun replica_addresses_follow_the_rust_rules() {
        for (ok in listOf("https://s3.example", "https://dav.example:8443/remote.php/dav/files/me/vault", "http://127.0.0.1:9000", "http://localhost:9000/b", "http://[::1]:9000")) {
            assertTrue(ok, ReplicaAddress.check(ok) is Checked.Valid)
        }
        for (bad in listOf("", "s3.example", "ftp://s3.example", "http://nas.lan/dav", "https://user:pass@host/dav", "https://host/dav?x=1", "https://host/dav#f", "https:///nohost", "https://ho st/")) {
            assertTrue(bad, ReplicaAddress.check(bad) is Checked.Invalid)
        }
        // Shown in the list: never userinfo, even if one slipped in before.
        assertEquals("https://host/dav", ReplicaAddress.redacted("https://user:pass@host/dav"))
        assertEquals("https://host:8443/x", ReplicaAddress.redacted("https://host:8443/x"))
    }
}
