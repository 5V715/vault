// SPDX-License-Identifier: MIT
package dev.vault.android

import dev.vault.android.pairing.PairingCoordinator
import dev.vault.android.replication.Replicator

/**
 * Debug builds only (instrumented tests run on debug): hooks the release
 * source set does not have (check-apks.sh asserts their absence there).
 */
object DebugHooks {
    @Volatile
    var pairingOverride: PairingCoordinator? = null

    @Volatile
    var replicatorOverride: Replicator? = null
}

/** Instrumented tests: the replicator WorkManager's jobs run (null restores the app's own). */
fun VaultApp.replaceReplicatorForTest(replicator: Replicator?) {
    DebugHooks.replicatorOverride = replicator
}

/**
 * Instrumented tests: route the service and the screens to a coordinator
 * built over test keys (null restores the app's own). It changes which keys
 * and vaults are used; it cannot approve anything.
 */
fun VaultApp.replacePairingForTest(coordinator: PairingCoordinator?) {
    DebugHooks.pairingOverride = coordinator
}
