// SPDX-License-Identifier: MIT
package dev.vault.android.ui

import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import dev.vault.android.vault.UserPresence
import dev.vault.android.vault.VaultManager
import java.text.DateFormat
import java.util.Date
import kotlinx.coroutines.launch
import uniffi.vault_ffi.VaultStats

/**
 * Vault detail: identity, stats, the re-wrap entry when K_auth was invalidated,
 * and delete (typed app id + screen-lock prompt). Replication targets and
 * settings arrive with background replication (Task 7).
 */
@Composable
fun VaultDetailScreen(
    manager: VaultManager,
    presence: UserPresence,
    entry: VaultEntry,
    onRewrap: () -> Unit,
    onDeleted: () -> Unit,
    onBack: () -> Unit,
) {
    var stats by remember { mutableStateOf<VaultStats?>(null) }
    var statsError by remember { mutableStateOf<String?>(null) }
    var needsRewrap by remember { mutableStateOf(false) }
    var confirmDelete by remember { mutableStateOf(false) }
    var typedAppId by remember { mutableStateOf("") }
    var deleteError by remember { mutableStateOf<String?>(null) }
    var deleting by remember { mutableStateOf(false) }
    val scope = rememberCoroutineScope()

    LaunchedEffect(entry.dir.id) {
        needsRewrap = try {
            manager.needsRewrap(entry.dir)
        } catch (e: Exception) {
            if (e is kotlinx.coroutines.CancellationException) throw e
            false
        }
        try {
            stats = manager.stats(entry.dir)
        } catch (e: Exception) {
            if (e is kotlinx.coroutines.CancellationException) throw e
            statsError = e.userMessage()
        }
    }

    VaultScaffold(entry.identity.appId, onBack) {
        Text(entry.identity.origin, style = MaterialTheme.typography.titleMedium)
        if (needsRewrap) {
            ErrorText("The unlock key was invalidated because the screen lock was removed. The vault can't be unlocked until it is re-wrapped.")
            Button(onClick = onRewrap, modifier = Modifier.fillMaxWidth()) { Text("Re-wrap with recovery key") }
        }
        HorizontalDivider()
        Text("Statistics", style = MaterialTheme.typography.titleMedium)
        val s = stats
        when {
            s != null -> {
                Text("Version (seq): ${s.seq?.toString() ?: "–"}")
                // Container figures are known only while unlocked (during a session).
                s.segments?.let { Text("Segments: $it") }
                s.storedBytes?.let { Text("Stored: ${formatBytes(it.toLong())}") }
                s.liveBytes?.let { Text("Live data: ${formatBytes(it.toLong())}") }
                Text("Last replication: ${s.lastReplicationMs?.let { DateFormat.getDateTimeInstance().format(Date(it.toLong())) } ?: "never"}")
                if (s.locked) Text("Locked. Size figures appear while a browser session is active.", style = MaterialTheme.typography.bodySmall)
            }
            statsError != null -> ErrorText(statsError!!)
            else -> Working("Reading…")
        }
        HorizontalDivider()
        if (!confirmDelete) {
            OutlinedButton(onClick = { confirmDelete = true }, modifier = Modifier.fillMaxWidth()) { Text("Delete vault…") }
        } else {
            Text(
                "Deleting removes this vault and its keys from the phone. Replicas are not touched; " +
                    "without a replica and the recovery key the data is gone for good.",
            )
            PlainField("Type the app id (${entry.identity.appId}) to confirm", typedAppId, { typedAppId = it }, KeyboardType.Ascii)
            deleteError?.let { ErrorText(it) }
            Button(
                onClick = {
                    deleting = true
                    scope.launch {
                        try {
                            if (presence.confirm("Delete ${entry.identity.appId}")) {
                                manager.delete(entry.dir)
                                onDeleted()
                            } else {
                                deleteError = "Not confirmed at the screen-lock prompt."
                            }
                        } catch (e: Exception) {
                            if (e is kotlinx.coroutines.CancellationException) throw e
                            deleteError = e.userMessage()
                        } finally {
                            deleting = false
                        }
                    }
                },
                enabled = typedAppId == entry.identity.appId && !deleting,
                colors = ButtonDefaults.buttonColors(containerColor = MaterialTheme.colorScheme.error),
                modifier = Modifier.fillMaxWidth(),
            ) { Text("Delete for good") }
            OutlinedButton(onClick = { confirmDelete = false; typedAppId = "" }, modifier = Modifier.fillMaxWidth()) { Text("Keep it") }
        }
    }
}

/** Metric (SI) units: 1 kB = 1000 B. */
private fun formatBytes(n: Long): String = when {
    n >= 1_000_000_000 -> "%.1f GB".format(n / 1e9)
    n >= 1_000_000 -> "%.1f MB".format(n / 1e6)
    n >= 1_000 -> "%.1f kB".format(n / 1e3)
    else -> "$n B"
}
