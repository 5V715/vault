// SPDX-License-Identifier: MIT
package dev.vault.android.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Button
import androidx.compose.material3.Checkbox
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import dev.vault.core.Checked
import dev.vault.core.ReplicaAddress
import uniffi.vault_ffi.ReplicaTarget

/**
 * The replica editor: S3 (endpoint, region, bucket, prefix, key id, secret) or
 * WebDAV (URL, user, password). Editing ([initial]) fills in everything but
 * the secret, which is typed again. Addresses are checked as vault-ffi will.
 */
@Composable
fun ReplicaForm(onSave: (ReplicaTarget) -> Unit, onCancel: () -> Unit, initial: ReplicaTarget? = null) {
    val s3Initial = initial as? ReplicaTarget.S3
    val davInitial = initial as? ReplicaTarget.WebDav
    var s3 by remember { mutableStateOf(davInitial == null) }
    var endpoint by remember { mutableStateOf(s3Initial?.endpoint ?: davInitial?.url ?: "") }
    var region by remember { mutableStateOf(s3Initial?.region ?: "") }
    var bucket by remember { mutableStateOf(s3Initial?.bucket ?: "") }
    var prefix by remember { mutableStateOf(s3Initial?.prefix ?: "") }
    var pathStyle by remember { mutableStateOf(s3Initial?.pathStyle ?: true) }
    var user by remember { mutableStateOf(s3Initial?.accessKeyId ?: davInitial?.user ?: "") }
    var secret by remember { mutableStateOf("") }
    var error by remember { mutableStateOf<String?>(null) }

    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            RadioButton(selected = s3, onClick = { s3 = true })
            Text("S3")
            RadioButton(selected = !s3, onClick = { s3 = false })
            Text("WebDAV")
        }
        if (s3) {
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
        } else {
            PlainField("Collection URL (https://…)", endpoint, { endpoint = it })
            PlainField("User", user, { user = it })
            SecretField("Password or app password", secret) { secret = it }
        }
        error?.let { ErrorText(it) }
        Button(
            onClick = {
                val missing = buildList {
                    if (endpoint.isBlank()) add(if (s3) "endpoint" else "URL")
                    if (s3 && region.isBlank()) add("region")
                    if (s3 && bucket.isBlank()) add("bucket")
                    if (user.isBlank()) add(if (s3) "access key id" else "user")
                    if (secret.isEmpty()) add(if (s3) "secret" else "password")
                }
                val addressProblem = (ReplicaAddress.check(endpoint) as? Checked.Invalid)?.reason
                if (missing.isNotEmpty()) {
                    error = "Missing: ${missing.joinToString(", ")}"
                } else if (addressProblem != null) {
                    error = addressProblem
                } else {
                    error = null
                    onSave(
                        if (s3) {
                            ReplicaTarget.S3(endpoint.trim(), region.trim(), bucket.trim(), prefix.trim(), user.trim(), secret, pathStyle)
                        } else {
                            ReplicaTarget.WebDav(endpoint.trim(), user.trim(), secret)
                        },
                    )
                    secret = ""
                }
            },
            modifier = Modifier.fillMaxWidth(),
        ) { Text("Save replica") }
        OutlinedButton(onClick = onCancel, modifier = Modifier.fillMaxWidth()) { Text("Cancel") }
    }
}
