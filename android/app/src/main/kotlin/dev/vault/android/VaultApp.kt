// SPDX-License-Identifier: MIT
package dev.vault.android

import android.app.Application
import dev.vault.android.keys.VaultKeys
import dev.vault.android.storage.VaultStorage
import dev.vault.android.vault.VaultManager

/** Process-wide objects: one VaultFactory (one Rust runtime) for the app. */
class VaultApp : Application() {
    val keys: VaultKeys by lazy { VaultKeys(this) }
    val storage: VaultStorage by lazy { VaultStorage(this, keys) }
    val manager: VaultManager by lazy { VaultManager(storage, keys) }
}
