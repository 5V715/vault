// SPDX-License-Identifier: MIT
package dev.vault.android

import android.app.Application
import java.util.concurrent.atomic.AtomicBoolean
import dev.vault.android.keys.VaultKeys
import dev.vault.android.storage.VaultStorage
import dev.vault.android.vault.VaultManager

/** Process-wide objects: one VaultFactory (one Rust runtime) for the app. */
class VaultApp : Application() {
    val keys: VaultKeys by lazy { VaultKeys(this) }
    val storage: VaultStorage by lazy { VaultStorage(this, keys) }
    val manager: VaultManager by lazy { VaultManager(storage, keys) }

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
