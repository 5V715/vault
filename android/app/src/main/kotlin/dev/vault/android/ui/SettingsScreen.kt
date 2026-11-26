// SPDX-License-Identifier: MIT
package dev.vault.android.ui

import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import dev.vault.android.settings.SettingsStore
import dev.vault.core.LockSettings
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** App-wide lock settings (every vault): auto-lock on screen off, idle timeout. Sealed when saved. */
@Composable
fun SettingsScreen(settings: SettingsStore, onBack: () -> Unit) {
    val current = remember { settings.lockSettings() }
    var screenOff by remember { mutableStateOf(current.lockOnScreenOff) }
    var idle by remember { mutableStateOf(current.idleTimeoutMinutes.toString()) }
    var message by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()

    VaultScaffold("Settings", onBack) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Switch(checked = screenOff, onCheckedChange = { screenOff = it })
            Text("Lock every vault when the screen turns off")
        }
        PlainField(
            "Idle timeout (minutes, 1–${LockSettings.MAX_IDLE_MINUTES})",
            idle,
            { v -> idle = v.filter(Char::isDigit).take(4) },
            KeyboardType.Number,
        )
        Text(
            "A session ends (and its vault locks) after this long without a request from the browser. " +
                "A new idle timeout applies from the next app start.",
            style = MaterialTheme.typography.bodySmall,
        )
        message?.let { Text(it) }
        Button(
            onClick = {
                val minutes = idle.toIntOrNull()
                if (minutes == null || minutes !in 1..LockSettings.MAX_IDLE_MINUTES) {
                    message = "The idle timeout must be 1 to ${LockSettings.MAX_IDLE_MINUTES} minutes."
                } else {
                    scope.launch {
                        message = try {
                            withContext(Dispatchers.IO) {
                                settings.saveLockSettings(LockSettings(lockOnScreenOff = screenOff, idleTimeoutMinutes = minutes))
                            }
                            "Saved."
                        } catch (e: Exception) {
                            if (e is kotlinx.coroutines.CancellationException) throw e
                            e.userMessage()
                        }
                    }
                }
            },
            modifier = Modifier.fillMaxWidth(),
        ) { Text("Save") }
    }
}
