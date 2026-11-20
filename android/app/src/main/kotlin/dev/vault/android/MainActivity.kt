// SPDX-License-Identifier: MIT
package dev.vault.android

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import dev.vault.android.ui.VaultTheme
import dev.vault.android.ui.VaultsScreen

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            VaultTheme {
                // Vault storage arrives in Task 5; until then the list is empty.
                VaultsScreen(vaults = emptyList())
            }
        }
    }
}
