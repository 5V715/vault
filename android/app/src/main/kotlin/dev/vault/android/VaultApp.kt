// SPDX-License-Identifier: MIT
package dev.vault.android

import android.app.Application
import java.util.concurrent.atomic.AtomicBoolean
import dev.vault.android.keys.VaultKeys
import dev.vault.android.pairing.PairingCoordinator
import dev.vault.android.storage.VaultStorage
import dev.vault.android.vault.VaultManager
import dev.vault.core.LockSettings

/** Process-wide objects: one VaultFactory (one Rust runtime) for the app. */
class VaultApp : Application() {
    val keys: VaultKeys by lazy { VaultKeys(this) }
    val storage: VaultStorage by lazy { VaultStorage(this, keys) }

    /**
     * App-wide lock settings (auto-lock on screen off, idle timeout). The
     * defaults for now; Task 7 keeps and edits them. The idle timeout reaches
     * vault-server through the factory, so a change applies after a restart.
     */
    val lockSettings: LockSettings get() = LockSettings()

    /** Debug builds may trust one extra CA for the LAN showcase (release: never). */
    val manager: VaultManager by lazy { VaultManager(storage, keys, lockSettings, DebugCa.extraCaPem(this, keys)) }

    private val defaultPairing: PairingCoordinator by lazy { PairingCoordinator(this, manager, storage) { lockSettings } }

    @Volatile
    private var testPairing: PairingCoordinator? = null

    /** Pairings and sessions (held while [dev.vault.android.pairing.SessionService] runs). */
    val pairing: PairingCoordinator get() = testPairing ?: defaultPairing

    /**
     * Instrumented tests only: route the service and the screens to a
     * coordinator built over test keys (null restores the app's own). It
     * swaps which keys and vaults are used; it cannot approve anything.
     */
    fun replacePairingForTest(coordinator: PairingCoordinator?) {
        testPairing = coordinator
    }

    private val swept = AtomicBoolean(false)

    /**
     * Removes leftovers of an interrupted create or restore, once per process
     * and before the first vault list loads (so before any create or restore of
     * this process can be running). Call off the main thread.
     */
    fun sweepOnce() {
        if (swept.compareAndSet(false, true)) storage.sweepIncomplete()
    }
}
