// SPDX-License-Identifier: MIT
package dev.vault.android

import android.os.Bundle
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import dev.vault.android.vault.BiometricPresence
import dev.vault.android.ui.CreateScreen
import dev.vault.android.ui.RestoreScreen
import dev.vault.android.ui.RewrapScreen
import dev.vault.android.ui.VaultDetailScreen
import dev.vault.android.ui.VaultEntry
import dev.vault.android.ui.VaultTheme
import dev.vault.android.ui.VaultsScreen
import dev.vault.core.VaultCatalog
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

private sealed interface Screen {
    data object List : Screen

    data class Create(val appId: String = "", val origin: String = "") : Screen

    data object Restore : Screen

    data class Detail(val entry: VaultEntry) : Screen

    data class Rewrap(val entry: VaultEntry) : Screen
}

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Every screen of this app can show a secret (recovery key, approval
        // code, vault identities): no screenshots, no recents thumbnail, not on
        // insecure displays.
        window.setFlags(WindowManager.LayoutParams.FLAG_SECURE, WindowManager.LayoutParams.FLAG_SECURE)
        enableEdgeToEdge()
        val app = application as VaultApp
        val presence = BiometricPresence(this)
        setContent {
            VaultTheme {
                App(app, presence)
            }
        }
    }
}

@Composable
private fun App(app: VaultApp, presence: BiometricPresence) {
    var screen by remember { mutableStateOf<Screen>(Screen.List) }
    var vaults by remember { mutableStateOf<List<VaultEntry>?>(null) }
    var reload by remember { mutableIntStateOf(0) }
    val catalog = remember { VaultCatalog(app.storage) }

    LaunchedEffect(reload) {
        vaults = withContext(Dispatchers.IO) {
            app.storage.vaultDirs()
                .mapNotNull { dir -> app.storage.identityOf(dir)?.let { VaultEntry(dir, it) } }
                .sortedWith(compareBy({ it.identity.appId }, { it.identity.origin }))
        }
    }
    fun home() {
        reload++
        screen = Screen.List
    }
    BackHandler(enabled = screen is Screen.Detail || screen is Screen.Rewrap) {
        screen = when (val s = screen) {
            is Screen.Rewrap -> Screen.Detail(s.entry)
            else -> Screen.List
        }
    }

    when (val s = screen) {
        Screen.List -> VaultsScreen(
            vaults = vaults,
            onOpen = { screen = Screen.Detail(it) },
            onCreate = { screen = Screen.Create() },
            onRestore = { screen = Screen.Restore },
        )
        is Screen.Create -> CreateScreen(
            manager = app.manager,
            catalog = catalog,
            presence = presence,
            prefillAppId = s.appId,
            prefillOrigin = s.origin,
            onDone = ::home,
            onBack = ::home,
        )
        Screen.Restore -> RestoreScreen(app.manager, presence, onDone = ::home, onBack = ::home)
        is Screen.Detail -> VaultDetailScreen(
            manager = app.manager,
            presence = presence,
            entry = s.entry,
            onRewrap = { screen = Screen.Rewrap(s.entry) },
            onDeleted = ::home,
            onBack = ::home,
        )
        is Screen.Rewrap -> RewrapScreen(
            manager = app.manager,
            presence = presence,
            dir = s.entry.dir,
            identity = s.entry.identity,
            onDone = { screen = Screen.Detail(s.entry) },
            onBack = { screen = Screen.Detail(s.entry) },
        )
    }
}
