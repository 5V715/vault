// SPDX-License-Identifier: MIT
package dev.vault.android

import dev.vault.android.pairing.PairingCoordinator

/**
 * Debug builds only (instrumented tests run on debug): hooks the release
 * source set does not have (check-apks.sh asserts their absence there).
 */
object DebugHooks {
    @Volatile
    var pairingOverride: PairingCoordinator? = null
}

/**
 * Instrumented tests: route the service and the screens to a coordinator
 * built over test keys (null restores the app's own). It changes which keys
 * and vaults are used; it cannot approve anything.
 */
fun VaultApp.replacePairingForTest(coordinator: PairingCoordinator?) {
    DebugHooks.pairingOverride = coordinator
}
