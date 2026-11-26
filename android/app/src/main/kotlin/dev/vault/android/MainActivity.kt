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
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.ui.Modifier
import androidx.lifecycle.LifecycleOwner
import dev.vault.core.Checked
import dev.vault.core.VaultIdentity
import dev.vault.android.vault.BiometricPresence
import dev.vault.android.ui.CreateScreen
import dev.vault.android.ui.PairingScreen
import dev.vault.android.ui.RestoreScreen
import dev.vault.android.ui.RewrapScreen
import dev.vault.android.ui.ScanScreen
import dev.vault.android.ui.SettingsScreen
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

    data object Scan : Screen

    data object Settings : Screen

    /** The pairing in progress or just ended ([dev.vault.android.pairing.PairingCoordinator.pairing]). */
    data object Pairing : Screen
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
                App(app, presence, this)
            }
        }
    }

    /**
     * Leaving the app (or the activity dying) while an approval is pending is
     * a denial; before the browser connected it cancels the pairing. Running
     * sessions are held by the foreground service and go on.
     */
    override fun onStop() {
        super.onStop()
        (application as VaultApp).pairing.backgrounded()
    }
}

@Composable
private fun App(app: VaultApp, presence: BiometricPresence, owner: LifecycleOwner) {
    var screen by remember { mutableStateOf<Screen>(Screen.List) }
    var vaults by remember { mutableStateOf<List<VaultEntry>?>(null) }
    var reload by remember { mutableIntStateOf(0) }
    val catalog = remember { VaultCatalog(app.storage) }
    val pairing = remember { app.pairing }
    val sessions by pairing.running.collectAsState()

    LaunchedEffect(reload) {
        vaults = withContext(Dispatchers.IO) {
            app.sweepOnce()
            val dirs = app.storage.vaultDirs()
            // A restored vault (or one WorkManager lost) gets its periodic replication.
            runCatching { app.replicator.ensureScheduled(dirs) }
            dirs.mapNotNull { dir -> app.storage.identityOf(dir)?.let { VaultEntry(dir, it) } }
                .sortedWith(compareBy({ it.identity.appId }, { it.identity.origin }))
        }
    }
    fun home() {
        reload++
        screen = Screen.List
    }
    BackHandler(enabled = screen is Screen.Detail || screen is Screen.Rewrap || screen is Screen.Scan || screen is Screen.Settings) {
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
            onScan = { screen = Screen.Scan },
            sessions = sessions,
            onStopSession = pairing::stopSession,
            footer = {
                OutlinedButton(onClick = { screen = Screen.Settings }, modifier = Modifier.fillMaxWidth()) { Text("Settings") }
                DebugCa.Setting(app.keys)
            },
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
            replicator = app.replicator,
            settings = app.settings,
            presence = presence,
            entry = s.entry,
            inSession = sessions.any { it.vaultId == s.entry.dir.id },
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
        Screen.Settings -> SettingsScreen(app.settings, onBack = ::home)
        Screen.Scan -> ScanScreen(
            pairing = pairing,
            lifecycleOwner = owner,
            onStarted = { screen = Screen.Pairing },
            onBack = ::home,
        )
        Screen.Pairing -> {
            val view by pairing.pairing.collectAsState()
            val v = view
            if (v == null) {
                LaunchedEffect(Unit) { home() }
            } else {
                PairingScreen(
                    view = v,
                    onApprove = { pairing.approve(presence) },
                    onDeny = pairing::deny,
                    onStop = pairing::stopSession,
                    onCreate = { offer ->
                        pairing.dismiss()
                        // Pre-filled only with a valid identity: an offer's text is
                        // browser-supplied and would show unescaped in the fields.
                        screen = if (VaultIdentity.of(offer.appId, offer.origin) is Checked.Valid) {
                            Screen.Create(offer.appId, offer.origin)
                        } else {
                            Screen.Create()
                        }
                    },
                    onRewrap = { id ->
                        pairing.dismiss()
                        val entry = vaults?.firstOrNull { it.dir.id == id }
                        screen = if (entry != null) Screen.Rewrap(entry) else Screen.List
                    },
                    onDone = {
                        pairing.dismiss()
                        home()
                    },
                )
            }
        }
    }
}
