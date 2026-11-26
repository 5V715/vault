// SPDX-License-Identifier: MIT
package dev.vault.android.vault

import kotlinx.coroutines.sync.Mutex

/**
 * One mutex per vault: its container takes one handle at a time, and every
 * writer of its anti-rollback state (pairing, replication, re-wrap,
 * compaction, delete) holds it while it has a handle open.
 */
class VaultLocks {
    private val locks = HashMap<String, Mutex>()

    @Synchronized
    fun of(vaultId: String): Mutex = locks.getOrPut(vaultId) { Mutex() }
}

/** The vault is in use (a session, a replication run, a compaction): try again shortly. */
class VaultBusyException : IllegalStateException("the vault is in use")
