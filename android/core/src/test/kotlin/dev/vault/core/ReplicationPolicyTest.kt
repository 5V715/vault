// SPDX-License-Identifier: MIT
package dev.vault.core

import dev.vault.core.ReplicationPolicy.Gate
import dev.vault.core.ReplicationPolicy.RunResult
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class ReplicationPolicyTest {
    private val wifi = ReplicationContext(network = Network.Unmetered, wifiOnly = true, sessionActive = false, deviceLocked = false)

    @Test
    fun replication_schedule_rules() {
        // Wi-Fi only.
        assertEquals(Gate.Run, ReplicationPolicy.gate(wifi))
        assertEquals(Gate.WaitForWifi, ReplicationPolicy.gate(wifi.copy(network = Network.Metered)))
        assertEquals(Gate.Run, ReplicationPolicy.gate(wifi.copy(network = Network.Metered, wifiOnly = false)))
        assertEquals(Gate.WaitForNetwork, ReplicationPolicy.gate(wifi.copy(network = Network.None, wifiOnly = false)))
        // An active session first (the existing replication policy), and K_bg
        // needs an unlocked device.
        assertEquals(Gate.WaitForSessionEnd, ReplicationPolicy.gate(wifi.copy(sessionActive = true)))
        assertEquals(Gate.WaitForDeviceUnlock, ReplicationPolicy.gate(wifi.copy(deviceLocked = true)))

        // Backoff: doubles from 30 s, capped at the periodic interval (6 h).
        assertEquals(30L, ReplicationPolicy.backoffSecs(0))
        assertEquals(60L, ReplicationPolicy.backoffSecs(1))
        assertEquals(120L, ReplicationPolicy.backoffSecs(2))
        assertEquals(6 * 3600L, ReplicationPolicy.backoffSecs(20))
        assertEquals(6 * 3600L, ReplicationPolicy.backoffSecs(Int.MAX_VALUE))

        // All replicas synced.
        assertEquals(
            RunResult.Ok(warnings = emptyList()),
            ReplicationPolicy.assess(listOf("s3" to ReplicaOutcome.Synced(5), "dav" to ReplicaOutcome.Adopted(5))),
        )
        // A transient failure retries with backoff.
        assertEquals(
            RunResult.Retry,
            ReplicationPolicy.assess(listOf("s3" to ReplicaOutcome.Synced(5), "dav" to ReplicaOutcome.Failed("timeout"))),
        )
        // A replica that was rolled back was repaired, but the user must hear of it.
        val rolled = ReplicationPolicy.assess(listOf("s3" to ReplicaOutcome.RolledBack(foundSeq = 3, lastSeenSeq = 5, seq = 6)))
        assertTrue(rolled is RunResult.Ok && rolled.warnings.single().contains("s3"))
        // A diverged replica was not touched: the user decides, no automatic retry.
        val diverged = ReplicationPolicy.assess(
            listOf("s3" to ReplicaOutcome.Diverged(replicaSeq = 9, primarySeq = 6), "dav" to ReplicaOutcome.Failed("x")),
        )
        assertTrue(diverged is RunResult.NeedsAttention && diverged.problems.single().contains("s3"))

        // The local vault is older than its high-water mark: an error, surfaced and
        // never retried (retrying cannot help, and nothing may overwrite a replica).
        val rollback = ReplicationPolicy.assessError(ReplicationError.Rollback)
        assertTrue(rollback is RunResult.NeedsAttention && rollback.problems.single().contains("rolled back"))
        assertTrue(ReplicationPolicy.assessError(ReplicationError.WrongKey) is RunResult.NeedsAttention)
        assertTrue(ReplicationPolicy.assessError(ReplicationError.KeyUnavailable) is RunResult.NeedsAttention)
        assertEquals(RunResult.Retry, ReplicationPolicy.assessError(ReplicationError.Busy))
        assertEquals(RunResult.Retry, ReplicationPolicy.assessError(ReplicationError.Network))
        assertEquals(RunResult.Retry, ReplicationPolicy.assessError(ReplicationError.DeviceLocked))
    }

    @Test
    fun no_targets_is_nothing_to_do() {
        assertEquals(RunResult.Ok(warnings = emptyList()), ReplicationPolicy.assess(emptyList()))
    }
}
