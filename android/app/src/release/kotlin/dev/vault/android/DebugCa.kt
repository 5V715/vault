// SPDX-License-Identifier: MIT
package dev.vault.android

import android.content.Context
import androidx.compose.runtime.Composable
import dev.vault.android.keys.VaultKeys

/**
 * Release builds trust only the bundled webpki roots: no extra CA, and no
 * setting to import one (the debug source set has the real one; check-apks.sh
 * asserts that the release APK does not contain it).
 */
object DebugCa {
    @Suppress("UNUSED_PARAMETER")
    fun extraCaPem(context: Context, keys: VaultKeys): String? = null

    @Suppress("UNUSED_PARAMETER")
    @Composable
    fun Setting(keys: VaultKeys) {
    }
}
