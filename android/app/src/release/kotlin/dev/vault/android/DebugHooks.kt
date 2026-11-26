// SPDX-License-Identifier: MIT
package dev.vault.android

import dev.vault.android.pairing.PairingCoordinator
import dev.vault.android.replication.Replicator

/** Release builds have no test hooks: nothing can replace the app's coordinator. */
object DebugHooks {
    val pairingOverride: PairingCoordinator? get() = null
    val replicatorOverride: Replicator? get() = null
}
