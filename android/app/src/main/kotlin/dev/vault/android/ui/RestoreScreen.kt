// SPDX-License-Identifier: MIT
package dev.vault.android.ui

import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Button
import androidx.compose.material3.Checkbox
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import dev.vault.android.storage.VaultDir
import dev.vault.android.vault.UserPresence
import dev.vault.android.vault.VaultManager
import dev.vault.core.VaultIdentity
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import uniffi.vault_ffi.ReplicaTarget

private enum class TargetType(val label: String) { S3("S3-compatible bucket"), WebDav("WebDAV (e.g. Nextcloud)") }

/**
 * Restore: target type → credentials → recovery key → restore (progress,
 * cancellable) → done. Cancelling aborts vault-ffi's restore and deletes the
 * partial vault and its keys.
 */
@Composable
fun RestoreScreen(manager: VaultManager, presence: UserPresence, onDone: () -> Unit, onBack: () -> Unit) {
    var type by remember { mutableStateOf(TargetType.S3) }
    var endpoint by remember { mutableStateOf("") }
    var region by remember { mutableStateOf("") }
    var bucket by remember { mutableStateOf("") }
    var prefix by remember { mutableStateOf("") }
    var pathStyle by remember { mutableStateOf(true) }
    var user by remember { mutableStateOf("") }
    var secret by remember { mutableStateOf("") }
    var recoveryKey by remember { mutableStateOf("") }
    var job by remember { mutableStateOf<Job?>(null) }
    var restored by remember { mutableStateOf<VaultIdentity?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()

    VaultScaffold("Restore a vault", onBack.takeIf { job == null }) {
        val done = restored
        when {
            done != null -> {
                Text("Restored", style = MaterialTheme.typography.titleLarge)
                Text("${done.appId} at ${done.origin}")
                Button(onClick = onDone, modifier = Modifier.fillMaxWidth()) { Text("Done") }
            }
            job != null -> {
                Working("Restoring… This downloads the whole vault.")
                OutlinedButton(onClick = { job?.cancel() }, modifier = Modifier.fillMaxWidth()) { Text("Cancel") }
            }
            else -> {
                Text("Where the vault's replica is", style = MaterialTheme.typography.titleMedium)
                TargetType.entries.forEach { t ->
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        RadioButton(selected = type == t, onClick = { type = t })
                        Text(t.label)
                    }
                }
                when (type) {
                    TargetType.S3 -> {
                        PlainField("Endpoint (https://…)", endpoint, { endpoint = it })
                        PlainField("Region", region, { region = it })
                        PlainField("Bucket", bucket, { bucket = it })
                        PlainField("Prefix (optional)", prefix, { prefix = it })
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Checkbox(checked = pathStyle, onCheckedChange = { pathStyle = it })
                            Text("Path-style URLs (MinIO, most self-hosted)")
                        }
                        PlainField("Access key id", user, { user = it })
                        SecretField("Secret access key", secret) { secret = it }
                    }
                    TargetType.WebDav -> {
                        PlainField("Collection URL (https://…)", endpoint, { endpoint = it })
                        PlainField("User", user, { user = it })
                        SecretField("Password or app password", secret) { secret = it }
                    }
                }
                RecoveryKeyField("Recovery key", recoveryKey) { recoveryKey = it }
                error?.let { ErrorText(it) }
                Button(
                    onClick = {
                        val target = when (type) {
                            TargetType.S3 -> ReplicaTarget.S3(
                                endpoint.trim(), region.trim(), bucket.trim(), prefix.trim(), user.trim(), secret, pathStyle,
                            )
                            TargetType.WebDav -> ReplicaTarget.WebDav(endpoint.trim(), user.trim(), secret)
                        }
                        error = null
                        job = scope.launch {
                            try {
                                restored = manager.restore(target, recoveryKey, presence)
                                recoveryKey = ""
                                secret = ""
                            } catch (e: kotlinx.coroutines.CancellationException) {
                                error = "Restore cancelled. Nothing was kept."
                            } catch (e: Exception) {
                                error = e.userMessage()
                            } finally {
                                job = null
                            }
                        }
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Restore") }
            }
        }
    }
}

/** Re-wrap after K_auth was invalidated: the recovery key, then a screen-lock prompt. */
@Composable
fun RewrapScreen(manager: VaultManager, presence: UserPresence, dir: VaultDir, identity: VaultIdentity, onDone: () -> Unit, onBack: () -> Unit) {
    var recoveryKey by remember { mutableStateOf("") }
    var working by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()
    VaultScaffold("Re-wrap with recovery key", onBack.takeIf { !working }) {
        Text("${identity.appId} at ${identity.origin}", style = MaterialTheme.typography.titleMedium)
        Text(
            "A new fingerprint or a changed screen lock invalidated this vault's unlock key. " +
                "Enter the recovery key to protect the vault with the current screen lock again.",
        )
        if (working) {
            Working("Opening the vault with the recovery key…")
        } else {
            RecoveryKeyField("Recovery key", recoveryKey) { recoveryKey = it }
            error?.let { ErrorText(it) }
            Button(
                onClick = {
                    working = true
                    error = null
                    scope.launch {
                        try {
                            manager.rewrap(dir, recoveryKey, presence)
                            recoveryKey = ""
                            onDone()
                        } catch (e: Exception) {
                            if (e is kotlinx.coroutines.CancellationException) throw e
                            error = e.userMessage()
                        } finally {
                            working = false
                        }
                    }
                },
                modifier = Modifier.fillMaxWidth(),
            ) { Text("Re-wrap") }
        }
    }
}
