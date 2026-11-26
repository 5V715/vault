// SPDX-License-Identifier: MIT
package dev.vault.android.replication

import dev.vault.android.pairing.SessionHooks

/**
 * The app's pairing hooks: a pairing makes a running replication of its
 * vault give way, and a session's end queues a run for what it committed.
 */
class ReplicationSessionHooks(private val replicator: () -> Replicator) : SessionHooks {
    override suspend fun pairingStarting(vaultId: String) = replicator().yieldTo(vaultId)

    override fun sessionEnded(vaultId: String) = replicator().afterSession(vaultId)
}
