// SPDX-License-Identifier: MIT
package dev.vault.android.ui

import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import dev.vault.android.replication.ReplicaInfo
import dev.vault.android.replication.Replicator
import dev.vault.android.settings.SettingsStore
import dev.vault.android.vault.UserPresence
import dev.vault.android.vault.VaultManager
import java.text.DateFormat
import java.util.Date
import dev.vault.core.DeleteConfirmation
import dev.vault.core.ReplicationPolicy
import dev.vault.core.ReplicationStatus
import dev.vault.core.Untrusted
import dev.vault.core.VaultSettings
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.vault_ffi.ReplicaTarget
import uniffi.vault_ffi.VaultStats

/**
 * Vault detail: identity, stats, the re-wrap entry when K_auth was invalidated,
 * replication (targets, status, last run, Wi-Fi only, replicate now),
 * compact, and delete (typed app id + screen-lock prompt).
 */
@Composable
fun VaultDetailScreen(
    manager: VaultManager,
    replicator: Replicator,
    settings: SettingsStore,
    presence: UserPresence,
    entry: VaultEntry,
    /** The vault pairs or serves a browser now. */
    inSession: Boolean,
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
    var replicas by remember { mutableStateOf<List<ReplicaInfo>>(emptyList()) }
    var editing by remember { mutableStateOf<Pair<String, ReplicaTarget>?>(null) }
    var compacting by remember { mutableStateOf(false) }
    var compactMessage by remember { mutableStateOf<String?>(null) }
    var status by remember { mutableStateOf<ReplicationStatus?>(null) }
    var vaultSettings by remember { mutableStateOf(VaultSettings()) }
    var adding by remember { mutableStateOf(false) }
    var replicating by remember { mutableStateOf(false) }
    var replicationError by remember { mutableStateOf<String?>(null) }
    var reload by remember { mutableIntStateOf(0) }

    LaunchedEffect(entry.dir.id, reload) {
        try {
            replicas = replicator.replicaInfos(entry.dir)
            withContext(Dispatchers.IO) {
                status = settings.status(entry.dir)
                vaultSettings = settings.vaultSettings(entry.dir)
            }
        } catch (e: Exception) {
            if (e is kotlinx.coroutines.CancellationException) throw e
            replicationError = e.userMessage()
        }
    }

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
        Text("Replication", style = MaterialTheme.typography.titleMedium)
        if (replicas.isEmpty()) Text("No replicas yet. Without one, losing the phone loses the vault.")
        for (r in replicas) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(Untrusted.display(r.description), modifier = Modifier.weight(1f))
                TextButton(onClick = {
                    scope.launch {
                        replicator.replicaForEdit(entry.dir, r.id)?.let { editing = r.id to it }
                    }
                }) { Text("Edit") }
                TextButton(onClick = {
                    scope.launch {
                        try {
                            replicator.removeReplica(entry.dir, r.id)
                        } catch (e: Exception) {
                            if (e is kotlinx.coroutines.CancellationException) throw e
                            replicationError = e.userMessage()
                        }
                        reload++
                    }
                }) { Text("Remove") }
            }
        }
        val edit = editing
        if (edit != null) {
            ReplicaForm(
                initial = edit.second,
                onSave = { target ->
                    scope.launch {
                        try {
                            replicator.updateReplica(entry.dir, edit.first, target)
                            editing = null
                        } catch (e: Exception) {
                            if (e is kotlinx.coroutines.CancellationException) throw e
                            replicationError = e.userMessage()
                        }
                        reload++
                    }
                },
                onCancel = { editing = null },
            )
        } else if (adding) {
            ReplicaForm(
                onSave = { target ->
                    scope.launch {
                        try {
                            replicator.addReplica(entry.dir, target)
                            adding = false
                        } catch (e: Exception) {
                            if (e is kotlinx.coroutines.CancellationException) throw e
                            replicationError = e.userMessage()
                        }
                        reload++
                    }
                },
                onCancel = { adding = false },
            )
        } else {
            OutlinedButton(onClick = { adding = true }, modifier = Modifier.fillMaxWidth()) { Text("Add a replica…") }
        }
        status?.let { st ->
            ReplicationStatusView(st)
            // A scheduler that never runs would otherwise go unnoticed.
            if (st.isStale(System.currentTimeMillis(), replicas.isNotEmpty())) {
                ErrorText("No successful replication in the last 24 hours. Check the replicas, or use Replicate now.")
            }
        }
        replicationError?.let { ErrorText(it) }
        Row(verticalAlignment = Alignment.CenterVertically) {
            Switch(
                checked = vaultSettings.wifiOnly,
                onCheckedChange = { on ->
                    scope.launch {
                        try {
                            replicator.setWifiOnly(entry.dir, on)
                        } catch (e: Exception) {
                            if (e is kotlinx.coroutines.CancellationException) throw e
                            replicationError = e.userMessage()
                        }
                        reload++
                    }
                },
            )
            Text("Background replication on Wi-Fi (unmetered) only")
        }
        if (replicas.isNotEmpty()) {
            Button(
                onClick = {
                    replicating = true
                    replicationError = null
                    scope.launch {
                        try {
                            // Runs in the replicator's scope: leaving the screen doesn't cancel it.
                            when (val r = replicator.replicateNow(entry.dir.id).await()) {
                                is Replicator.Report.Waiting -> replicationError = waitingText(r.gate)
                                is Replicator.Report.Skipped -> replicationError = "Nothing to replicate (${r.reason})."
                                is Replicator.Report.Done -> Unit
                            }
                        } catch (e: Exception) {
                            if (e is kotlinx.coroutines.CancellationException) throw e
                            replicationError = e.userMessage()
                        } finally {
                            replicating = false
                            reload++
                        }
                    }
                },
                enabled = !replicating && !inSession,
                modifier = Modifier.fillMaxWidth(),
            ) { Text(if (inSession) "Replicate now (after the session)" else "Replicate now") }
            if (replicating) Working("Replicating…")
        }
        HorizontalDivider()
        Text("Compaction", style = MaterialTheme.typography.titleMedium)
        Text(
            "Rewrites the vault without its dead data. The vault is unlocked (fingerprint or PIN) only while it compacts.",
            style = MaterialTheme.typography.bodySmall,
        )
        OutlinedButton(
            onClick = {
                compacting = true
                compactMessage = null
                scope.launch {
                    compactMessage = try {
                        "Compacted (seq ${manager.compactNow(entry.dir, presence)})."
                    } catch (e: Exception) {
                        if (e is kotlinx.coroutines.CancellationException) throw e
                        e.userMessage()
                    } finally {
                        compacting = false
                        reload++
                    }
                }
            },
            enabled = !compacting && !inSession,
            modifier = Modifier.fillMaxWidth(),
        ) { Text(if (inSession) "Compact now (after the session)" else "Compact now") }
        if (compacting) Working("Compacting…")
        compactMessage?.let { Text(it) }
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
                            if (inSession) {
                                deleteError = "Stop the vault's session first."
                            } else {
                                when (manager.deleteConfirmed(entry.dir, entry.identity, typedAppId, presence)) {
                                    VaultManager.DeleteResult.Deleted -> onDeleted()
                                    VaultManager.DeleteResult.NotConfirmed -> deleteError = "Not confirmed at the screen-lock prompt."
                                    VaultManager.DeleteResult.AppIdNotTyped -> deleteError = "Type the app id exactly."
                                    VaultManager.DeleteResult.Busy ->
                                        deleteError = "The vault is in use (a session or a replication). Try again in a moment."
                                }
                            }
                        } catch (e: Exception) {
                            if (e is kotlinx.coroutines.CancellationException) throw e
                            deleteError = e.userMessage()
                        } finally {
                            deleting = false
                        }
                    }
                },
                enabled = DeleteConfirmation.matches(typedAppId, entry.identity.appId) && !deleting,
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

@Composable
private fun ReplicationStatusView(s: ReplicationStatus) {
    fun time(ms: Long?) = ms?.let { DateFormat.getDateTimeInstance().format(Date(it)) } ?: "never"
    Text("Last run: ${time(s.lastRunMs)} · last success: ${time(s.lastOkMs)}${s.seq?.let { " (seq $it)" } ?: ""}")
    Text(
        when (s.state) {
            ReplicationStatus.State.Never -> "Not replicated yet."
            ReplicationStatus.State.Ok -> "Replicas are up to date."
            ReplicationStatus.State.Waiting -> "Waiting (a session, the network or an unlocked phone)."
            ReplicationStatus.State.Retrying -> "Retrying after ${s.failedRuns} failed run(s)."
            ReplicationStatus.State.NeedsAttention -> "Needs your attention: background replication is stopped."
        },
    )
    // Rust's and the policy's text; replica errors may quote a provider: escaped.
    s.problems.forEach { ErrorText(Untrusted.display(it)) }
    s.warnings.forEach { ErrorText("Warning: ${Untrusted.display(it)}") }
}

private fun waitingText(gate: ReplicationPolicy.Gate): String = when (gate) {
    ReplicationPolicy.Gate.WaitForSessionEnd -> "The vault is in a session; it replicates when the session ends."
    ReplicationPolicy.Gate.WaitForDeviceUnlock -> "Unlock the phone first."
    ReplicationPolicy.Gate.WaitForNetwork -> "No network; it replicates once there is one."
    ReplicationPolicy.Gate.WaitForWifi -> "Waiting for Wi-Fi."
    ReplicationPolicy.Gate.Run -> "Replicating."
}
